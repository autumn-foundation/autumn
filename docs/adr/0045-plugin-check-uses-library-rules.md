# ADR 0045: `autumn plugin-check` Uses the Library Conformance Rules

- Status: Accepted
- Date: 2026-11-10
- Deciders: Autumn maintainers
- Tags: plugins, conformance, cli

## Context

Issue [#2828](https://github.com/autumn-foundation/autumn/issues/2828).

`autumn plugin-check` (`autumn-cli/src/plugin_check.rs`) and
`autumn_web::plugin_conformance` had two copies of one rule set. Each copy
had its own report types and its own check functions. The copies drifted:

1. The CLI had no intentional-root exemption.
2. The CLI did not fail `--no-routes` together with `--prefix`.
3. The CLI gave other diagnostics for collisions and sensitive routes.
4. With `--no-routes`, the CLI counted routes under one name of the plugin.
   The library counts routes under every name.
5. Only the CLI ran the route checks under the contract's `registered_as`,
   and only the CLI failed routes under two names of one plugin.

Earlier changes fixed items 1 and 2 by hand. Items 3 to 5 remained.

The CLI reads routes from a JSON dump. There, the route source is a string.
The library `RouteInfo` uses the `RouteSource` enum. `RouteSource` already
reads `plugin:<name>` when it deserializes.

`plugin_conformance` is a stable plugin surface.

## Options

1. The CLI converts each dumped route to the library `RouteInfo`. Then it
   calls the library check functions.
2. The library checks become generic over a route trait.
3. Move the rules to a new crate.
4. Keep two copies. Add more name-scan tests.

## Decision

Option 1.

- `RouteSource::from_wire` reads the dump string. `Deserialize` uses it.
- New `run_route_checks(config, routes)` runs every route check:
  `route-attribution`, `route-prefix`, `route-collision`,
  `sensitive-surfaces` and `duplicate-registration`. `run_conformance`
  calls it, then adds `experimental-surface`.
- `run_route_checks` holds the registered-name rule and the split-name rule
  that were in the CLI. They apply when the config has a contract.
- The CLI uses the library `CheckStatus`, `CheckResult`, `SensitiveRoute`
  and `ConformanceReport`.
- `PluginCheckReport` wraps the library report with
  `#[serde(flatten)]`. It adds the data the plugin index reads: `contract`,
  `autumn_web`, `prefix` and `intentional_root`. The JSON shape does not
  change.
- The CLI builds a `ConformanceConfig` from its flags and calls
  `run_route_checks`. It keeps only what needs the built binary or the CLI
  flags: `installability`, `plugin-contract`, the contract-dump outcomes,
  `--deny-experimental`, the omitted-router check, and the `--no-routes`
  gate below.
- `route_checks_match_the_library` runs the CLI and `run_conformance` on
  the same routes, with and without a contract. It fails when the CLI skips
  a library check or gives a different result.

```mermaid
flowchart LR
    D[Route dump JSON] --> C[autumn plugin-check]
    C -->|RouteSource::from_wire| R[library RouteInfo]
    R --> L[run_route_checks]
    L --> P[PluginCheckReport]
    C -->|installability, plugin-contract, CLI-only checks| P
```

Option 2 adds a trait engine for one caller. Option 3 adds a crate for one
module. Option 4 does not stop drift.

## Consequences

- A rule change in `plugin_conformance` changes the CLI too.
- The public library API grows by `run_route_checks` and
  `RouteSource::from_wire`. It does not break.
- `run_conformance` with a contract now follows `registered_as`, and it
  fails routes under two names of one plugin.
- Some CLI output text changes. See the changelog.
- `--no-routes` still needs a contract with `registered_as` to skip. The
  library does not. `docs/plugins.md` describes this difference.
