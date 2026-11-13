### Breaking Changes

- **Breaking:** `autumn_web::cluster` needs the new `cluster` feature
  (issue #2795, ADR 0045). The feature is in `default`, so a default build
  does not change. An app with `default-features = false` that uses the
  cluster adds `"cluster"`. Without the feature, `[cluster]` parses, and
  `enabled = true` fails boot. The CLI still reads such a config
  ([migration guide](docs/migrations/next.md#features-the-cluster-module-is-optional)).

### Performance

- **build:** `autumn-web` compiles 13.4% less LLVM IR for the no-DB
  daemon starter (issue #2795, ADR 0045). Config is read through one serde
  deserializer, not one per toml access type, and the schema walk uses the
  same one. The `cluster` module is gated. A default build gets the
  deserializer part of the gain.

### Changed

- **config:** a type error in `autumn.toml` names the full key, for example
  ``in `server.port` ``. Before, `AutumnConfig::load` showed a line of the
  merged config, which is not a file the app wrote. `AutumnConfig::load_from`
  errors also give the key, not the line. Syntax errors keep their line.
  Without `strict_config`, `load` no longer prints the merged config and
  parses it again.
- **dev-loop:** the gated cold-start budget (`ColdStartHello`) is now p95
  125 s and max 150 s, from p95 130 s and max 160 s. The scheduled CI runs
  measured 89-104 s after the ADR 0027 gates.
