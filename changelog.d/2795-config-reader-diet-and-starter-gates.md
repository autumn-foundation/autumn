### Breaking Changes

- **Breaking:** an `autumn.toml` section written as an array fails boot
  (issue #2795, ADR 0049). The config reader now reads a struct from a table
  only. Before, `server = [8080, "0.0.0.0"]` loaded by field position. An
  array of tables and an array value do not change
  ([migration guide](docs/migrations/next.md#config-a-section-is-a-table-not-an-array)).
- **Breaking:** `autumn_web::plugin_conformance`, `autumn_web::runtime_config`
  and axum's WebSocket extractor (`reexports::axum::extract::ws`) need the
  `plugin-conformance`, `runtime-config` and `axum-ws` features (issue
  #2795, ADR 0049). The three are in `default`, and `ws` turns `axum-ws` on.
  Only a build with `default-features = false` that uses one of them changes
  ([migration guide](docs/migrations/next.md#features-plugin-conformance-runtime-config-and-axum-ws)).

### Added

- **dev-loop:** the latency and cold-start reports have `cpu_count` and
  `cpu_model` (issue #2795). Two CI runners of one kind can have different
  CPUs. The report now shows which one made the samples.

### Performance

- **build:** `autumn-web` compiles less code for the config reader in every
  app (issue #2795, ADR 0049). The LLVM IR of the no-DB daemon starter's
  feature set fell from 2,733,293 to 2,432,428 lines (11.0%).
- **build:** an app from `autumn new --daemon` does not compile the plugin
  conformance harness, the runtime-config store or the WebSocket stack
  (`tungstenite` and 8 more crates). On the test box, the median of 6 cold
  builds fell from 74.3 s to 71.6 s.

### Changed

- **capacity:** a value of the wrong type in `capacity.lock` names the key,
  for example ``in `envelope.admission_limit` ``. Before, the error showed
  the line. A syntax error keeps its line.
- **capacity:** `autumn calibrate --seed` accepts at most
  9223372036854775807, the largest TOML integer. A `capacity.lock` with a
  larger seed does not load: run `autumn calibrate` again.
