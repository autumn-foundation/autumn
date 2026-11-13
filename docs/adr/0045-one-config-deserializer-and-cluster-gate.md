# ADR 0045: One Config Deserializer, and a `cluster` Gate

- Status: Accepted (2026-11-11; the differences from `toml` changed in ADR 0049)
- Date: 2026-11-11
- Deciders: Autumn maintainers
- Tags: build-time, config, features, cold-start

## Terms

- **IR**: LLVM intermediate representation that rustc makes for a crate.
- **Copy**: one monomorphized instance of a generic function.
- **Daemon starter**: the app that `autumn new --daemon` makes. It has no DB.

## Context

Issue #2795: the cold-start build is slow, and `autumn-web` is the largest
compile unit. ADR 0027 gated 16 modules. The scheduled CI gate then measured
89-104 s, against a budget of p95 130 s.

New data, daemon feature set, one 4 vCPU box:

- `autumn-web` is 35.5 s of a 76.6 s cold scaffold build. It cannot start
  until `autumn-macros` is built.
- `-Z self-profile`: LLVM work (emit, passes, codegen) is the largest part.
  Type check and borrow check are smaller.
- `cargo llvm-lines`: 3,462,902 IR lines. `Deserialize` code for
  `autumn-web` types is 25.6% of it.
- `toml::from_str` makes one copy of each config struct's visitor for each
  toml access type: table, array, datetime and span. That is 15.6% of the IR.
  The schema walk (`SchemaDeserializer`) makes one more copy: 4.9%.
- `AutumnConfig::load` merged the layers into a `toml::Value`, printed it as
  text, and parsed the text again. A type error then pointed at a line of
  that text, which is not a file the user wrote.
- `cluster` is experimental, has five call sites, and is 3.1% of the IR.

## Planning

Brainstorming: one deserializer for config; put the schema walk on it; gate
more modules (`cluster`, `shadow`, `capacity`, `aeo`); split `autumn-web`
into crates; prebuilt artifacts; reorder Cargo jobs; tighten the budget.

Reverse brainstorming (how to make it worse): change what a config means
(datetimes, enums, `flatten`, `untagged`, integer range); lose the error
location; leave one `toml::from_str` into a config type, so the copies come
back; gate a module that an app uses; tighten the budget until the gate
fails on noise.

Six hats:

- White (facts): the data above.
- Red (feeling): a config change feels risky. It touches every app.
- Black (risk): a silent change in how config reads. An app with
  `default-features = false` breaks. A flaky gate.
- Yellow (gain): the deserializer change helps every app, also with default
  features. The error message gets better.
- Green (ideas): split the crate later. That is the larger lever.
- Blue (process): test first. Measure each step. Keep a step only if the IR
  falls.

## Decision

1. `config::value_de::ConfigDe` reads config. It has two arms. `Value` reads
   a merged `toml::Value`. `Schema` is the schema walk. Both use one map
   type, one sequence type and one enum type. Each config struct compiles
   one visitor copy.
2. `AutumnConfig::load` and `load_from` read through `ConfigDe`. Without
   `strict_config`, `load` does not print and parse the merged config again.
   A type error names its key: ``in `server.port` ``. `ConfigError::Parse`
   keeps its type. A syntax error keeps its line, because each file is
   parsed into a `toml::Table` first.
3. A datetime goes to a visitor as a one-entry map with toml's datetime key.
   `toml::Value` and `toml_datetime::Datetime` read it as before.
4. The public `SchemaDeserializer` keeps its API. Each method runs the
   `Schema` arm of `ConfigDe`.
5. `cluster` is a default-on feature, as the ADR 0027 gates. Without it,
   `[cluster]` still parses. Boot fails on `enabled = true`, because a node
   that does not join is a fault. `push` also fails boot when its section is
   set without the feature. `ClusterConfig::validate` does not check the
   feature: the CLI builds without it and must still read the app's config.
6. The `ColdStartHello` budget is p95 125 s and max 150 s. The gate checks
   p95 and max only, and with 3 runs p95 is the slowest sample. p95 is about
   20% above the slowest CI sample after ADR 0027.

Tests:

- `config::value_de` compares `ConfigDe` with `toml::from_str` on every
  example config and on the profile defaults, and tests each serde shape.
- A test scans `config.rs`: outside tests, `toml::from_str` names
  `toml::Table` or `toml::Value`.
- `compile_gate_features` lists `cluster`.

## Options not selected

- Deserialize from `toml::Value` with its own `Deserializer`. It still makes
  a datetime copy of each struct visitor.
- Gate `aeo` and `agent_authority`. They are 7.4% of the IR, but the router,
  MCP and the macros use them. That is a larger change.
- Raise the budget only. That accepts the cost and does not lower it.

## Consequences

- IR lines, daemon feature set: 3,462,902 before, 3,000,123 after (-13.4%).
  The same build with `cluster` on has 3,162,592 (-8.7%). That is the
  config change alone.
- Build time on one 4 vCPU box, interleaved samples, medians: `autumn-web`
  alone 39.9 s to 37.8 s (-5.3%, n = 4). A cold scaffold build 79.2 s to
  77.7 s (-1.8%, n = 5). The scaffold change is less than the run-to-run
  noise. LLVM work runs on all cores, and the front end does not. So fewer
  IR lines give a smaller time gain.
- The schema from `get_schema_keys()` is the same as before (111 tables).
- An app with `default-features = false` that uses the cluster adds
  `"cluster"`. See `docs/migrations/next.md`.
- A program that parsed the `TOML parse error at line` text of a config
  type error must read the key path instead.
- Small differences from `toml::from_str` remain. Map keys are always
  strings, and a tuple variant cannot be written as a table. No config type
  uses either.
- The 60 s / 90 s target is still open. A split of `autumn-web` is the next
  lever.
