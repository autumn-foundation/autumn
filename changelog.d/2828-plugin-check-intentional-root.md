### Fixed

- **🪞 Echo findings (#2828): `autumn plugin-check` gains `--intentional-root`
  (repeatable), the CLI-side spelling of the library API's
  `ConformanceConfig::intentional_root_routes`:** the CLI's `route-prefix`
  check had no equivalent of the library harness's intentional-root
  exemption, so a plugin author who legitimately declares a root-level route
  (via the same API their crate's own integration test uses) got a false
  `route-prefix` FAIL running the CLI in CI. Declared paths (exact match)
  are now exempt; the fail message names the exemption. The sandboxed
  `autumn plugin inspect` lane passes an empty list — the WASM manifest has
  no intentional-root spelling yet, left as a follow-up.
  The JSON report lists the exempted paths under `intentional_root`, and
  `autumn plugin index record` refuses a passing report that relied on them,
  since an index listing cannot record the exemption for reverification.
