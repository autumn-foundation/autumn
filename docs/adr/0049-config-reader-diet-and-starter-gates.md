# ADR 0049: A Smaller Config Reader, and Three Starter Gates

- Status: Accepted
- Date: 2026-11-13
- Deciders: Autumn maintainers
- Tags: build-time, features, cold-start, config

## Terms

- **Daemon starter**: the app that `autumn new --daemon` makes. It has no DB.
- **Unit-second**: one second of one Cargo compile unit (one crate build).
- **Config reader**: `ConfigDe` in `autumn/src/config/value_de.rs`, the one
  serde deserializer for `autumn.toml` (ADR 0045).
- **Copy**: one compiled instance of a generic function for one type.

## Context

Issue #2795: the cold build of the daemon starter is slow. The target is p95
60 s and max 90 s on the CI gate. After ADR 0046, two CI runs measured
50.4-52.8 s on one runner and 75.8-76.9 s on another. The slower runner needs
about 17 s more.

New data, daemon feature set, one 4 vCPU box, `CARGO_INCREMENTAL=0` as in CI:

- A cold build takes 72.0-73.5 s. Dependencies end at about 36.6 s and use
  all four cores (about 143 unit-seconds). `autumn-web` takes 33.5 s, mostly
  on one core. The app takes 2.5 s.
- A short form of the ADR 0046 model fits: `T = D/4 + W + A`. `D` is the
  unit-seconds of the dependencies, `W` is the time of `autumn-web`, and `A`
  is the time of the app. The first phase is full: the cores are idle for
  3.7 core-seconds in 36 s. So one unit-second less in a dependency saves
  0.25 s, and one second less in `autumn-web` saves 1 s.
- `autumn-web` has no hot function. Its LLVM IR is 2,733,293 lines in 79,170
  copies. The config module is 18.9% of the IR. The next module is 5.9%.
- Inside the config share, the reader made these copies for each config
  type: the code that reads a table key through the full value reader (134
  field and variant visitors), the code that reads a struct from an array
  and from each scalar (108 struct visitors), and the code that records the
  fields for the schema.
- axum's `ws` feature was on in the workspace entry, so each app compiled
  `tungstenite` and 8 more crates (11 compile units): 3.2 unit-seconds. Each
  use in `autumn-web` is behind the `ws` feature, which the daemon starter
  does not name.
- `plugin_conformance` (the harness of plugin tests and of
  `autumn plugin-check`) and `runtime_config` (a key store that only app
  code arms) compile in each app: 0.95% and 0.71% of the IR.

## Planning

Four groups of options were measured first: a config crate, more module
gates, the config reader, and dependency features.

### Brainstorm

Options, with the saving that the model gives on the test box:

| Option | Saving | Helps |
|--------|--------|-------|
| Smaller config reader | 1.0-1.5 s | every app |
| `axum-ws` feature | 0.8 s | starter |
| `plugin-conformance` gate | 0.4 s | starter |
| `runtime-config` gate | 0.3 s | starter |
| brotli behind a feature | 0.95 s | starter |
| `shadow` and `fault-injection` gates | 1.0 s | starter |
| validator `card` behind a feature | 0.4-0.75 s | starter |
| `rand_chacha` to `rand`'s own ChaCha | 0.9 s | every app |
| `autumn-config` crate | about 3 s | every app |

Each small option alone is less than the noise of one build. Together, as
one change with one checklist, they are measurable.

### Reverse brainstorm

How would this step make the build, or the framework, worse?

- A config file that loaded stops loading, and no note says why. So the one
  reader change that a user can see has a test that shows the difference
  from `toml`, a migration section and a breaking entry.
- A gate removes a module and a docs build loses its links. Two lists name
  the docs features (`docs.rs` metadata, `scripts/check-docs.sh`). A spec
  now fails when one of the three gates is missing from one of the lists.
- The CLI builds `autumn-web` with default features off and uses the
  conformance module. A workspace build hides a missing feature there. A
  spec reads the CLI manifest.
- No CI build compiled the test targets with one of these three gates off,
  so a missing `cfg` on a test was invisible. The new lane runs the specs
  with the three features off.
- A feature goes away from a manifest and comes back unseen later. The lane
  fails when the starter's dependency tree has a WebSocket crate, and fails
  when the default tree has none (an empty check).
- brotli off sends an undecoded `br` body of a shadow target into the byte
  comparison: a false divergence. Not in this step.
- Dropping validator `card` can remove regex Unicode tables that the log
  filter uses for a field pattern. Not in this step.

### Six hats

- **White (facts):** the numbers in Context. The IR totals are measured. The
  seconds for the gates come from the model.
- **Red (feeling):** a refused config form is alarming, even when no guide
  shows it. Three more feature names are acceptable. Eight are not. A move
  of 41,000 config lines for no gain in its first PR is a rebase trap.
- **Black (risk):** the struct change differs from `toml`. A gate without a
  compile error (a boot refusal) needs more design than a gate with one.
- **Yellow (benefit):** the reader work helps every app, the default DB app
  too. The gates cost a default build nothing.
- **Green (ideas):** move each generic step of the reader into a function
  that is not generic. Read `capacity.lock` with the same reader.
- **Blue (process):** red test, green change, one commit for each step, an
  IR count in each reader commit, then interleaved cold builds of trunk and
  head.

## Decision

1. **A key has its own deserializer.** A table key or a variant name goes to
   `KeyDe`, not through the value reader. As in `toml`, a bool, integer or
   char key is parsed from the key text. An enum as a key is a unit variant.
2. **A struct is a table.** `deserialize_struct` accepts a table, and a
   datetime as its one-entry table. An array or a scalar is an error that
   names the struct and the key. This is the one visible change: `toml`
   reads `server = [8080, "0.0.0.0"]` by field position. A struct inside an
   untagged enum is read through serde's buffer and still accepts an array.
3. **The steps are not generic.** The schema field recorder, the map step,
   the enum entry and the newtype variant are plain functions. Each copy for
   a type is a short call.
4. **`capacity.lock` uses the config reader.** The text is parsed one time
   into a raw table. A test fails on a typed `toml::from_str` in `config.rs`
   or `capacity.rs`.
5. **Three default-on features:** `plugin-conformance` and `runtime-config`
   gate their modules. `axum-ws` owns `axum/ws`, and `ws` turns it on. The
   loss in each case is a compile error that names the path. None has a
   config section, so there is no boot check.
6. **One lane.** `scripts/check-daemon-starter-features.sh` checks the
   starter's features and dependency tree, compiles its feature set and
   the CLI alone, and runs the specs with the three features off.

7. **The report names the CPU.** The dev-loop and cold-start reports have
   `cpu_count` and `cpu_model`. Two CI runs on `ubuntu-latest` differed by
   25 s, and the reports gave no cause.

Decision 5 reverses two options that ADR 0046 did not select: a
`plugin_conformance` gate, and `axum/ws` as a default-on feature. ADR 0046
judged each one alone as too small. Here they go in one change with one
lane.

This changes one sentence of
[ADR 0045](0045-one-config-deserializer-and-cluster-gate.md). "Map keys are
always strings" is no longer true. The known differences from `toml` are
now: a struct is a table, and a tuple variant cannot be written as a table.

## Options not selected

- **`autumn-config` crate, now.** About 3 s for every app after this step.
  The move is 41,000 lines with about 20 forwarded features and 11 methods
  that return runtime types. It needs two changes: the config types of other
  modules move under `config/` first, then the lift to a crate.
- **`shadow`, `fault-injection`, `web-bot-auth` gates.** Each has a config
  section, so each needs a boot refusal, and two of them change a warning
  into a failure. They go together in the next step, with one design.
- **brotli behind a feature.** The starter would serve gzip where it served
  `br`, and the shadow transport needs an explicit error for a `br` body. It
  goes with the `shadow` gate.
- **validator `card` behind a feature.** First a test that a log filter with
  a `\d` field pattern still parses in a build without it.
- **`rand_chacha` to `rand`'s ChaCha.** It changes the generator of
  `entropy`, `fake` and `sim`. First a test of today's ChaCha8 bytes.
- **Pair array items with field names in the struct path.** It keeps the
  `toml` behavior at the same IR. Not selected: no guide shows the form, and
  an error with the key path is easier to read than a wrong field.
- **A second boundary parser in place of `multer`.** Rejected in ADR 0046.
  The reason holds.
- **A gate for the test harness (`autumn_web::test`).** The first
  `cargo test` of an app would build `autumn-web` again.
- **validator `derive` behind a feature.** A new app validates a form early.

## Consequences

Measured, daemon feature set, same box:

- `autumn-web` IR: 2,733,293 lines to 2,432,428 after decisions 1-4 (11.0%
  less), in 72,899 copies (7.9% less).
- `autumn-web` IR after decision 5: 2,377,914 lines (13.0% less than
  before this ADR), in 71,266 copies (10.0% less).
- Compile units of the starter: 325 to 314. Nine crates left its tree:
  `tungstenite`, `tokio-tungstenite`, `data-encoding`, `sha1`, `base64`
  0.22, `rand` 0.9, `rand_chacha` 0.9, `rand_core` 0.9 and `getrandom` 0.3.
- Cold build, 6 interleaved pairs of trunk and this change: the median fell
  from 74.3 s to 71.6 s (2.7 s less). Trunk: 73.1-76.0 s. This change:
  69.7-76.6 s. `autumn-web` alone: 33.3 s to 31.5 s. The pairs ran later
  than the samples in Context, when the host was slower.
- Decisions 1-4 alone, 5 pairs on a noisy host: `autumn-web` was 1.3 s
  faster (median of the pair differences). The totals did not show it.
- CI gate, manual runs of 5 samples. This change: 58.0-60.9 s on an
  `AMD EPYC 9V74` runner (p95 60.9 s), and 76.2-78.0 s on an `AMD EPYC 7763`
  runner (p95 78.0 s). Trunk, two runs on the same base commit: 62.0-65.0 s
  and 64.4-66.9 s. The trunk report has no CPU field. If those two runs were
  on the faster kind, the gain there is about 4 s. No trunk run came from
  the slower kind, so CI does not show the gain there: ADR 0046 measured
  75.8-76.9 s in its slow group.
- A default DB app gains only from decisions 1-4.

Costs:

- A config section written as an array fails boot. See the migration guide.
- A build with default features off that uses one of the three gated items
  adds a feature name.
- A type error in `capacity.lock` names the key and not the line.
- A `capacity.lock` with an integer above `i64::MAX` does not load. Only an
  explicit `--seed` can be that large. `autumn calibrate` now refuses such
  a seed and does not write such a contract.
- The `test-features` CI job has one more lane.

The cold-start budget does not change. The measure job does not run on a
pull request, and the two runner kinds differ by about 17 s.

## Roadmap to 60 s / 90 s

On the CI gate, the `AMD EPYC 9V74` runner is 1 s from the 60 s target, and
the `AMD EPYC 7763` runner still needs about 18 s. The work of the build is
now about
179 unit-seconds on the test box (148 for the dependencies and 31.5 for
`autumn-web`, medians of the 6 samples). On four cores that is about 45 s at
best, and today's build takes 71.6 s, because `autumn-web` compiles mostly on
one core. In order of gain for each line changed:

1. The next gates, as one design with a boot refusal: `shadow`,
   `fault-injection`, `web-bot-auth`, and brotli. About 2.5 s for the
   starter.
2. validator `card` and `rand_chacha`, each after its test. About 1.5 s.
3. The config types of other modules move under `config/`, then the
   `autumn-config` crate. About 3 s for every app.
4. Gates for `aeo` and `agent_authority`. About 3 s. This changes what the
   starter is, so it needs its own decision.
5. A split of `autumn-web` into crates that compile in parallel. This is the
   only step that shortens the single-core phase by more than a few seconds.
