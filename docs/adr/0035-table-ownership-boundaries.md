# ADR 0035: Check Table Ownership on the Architecture Graph

- Status: Accepted
- Date: 2026-11-03
- Deciders: Autumn maintainers
- Tags: architecture, graph, service-extraction

## Context

Issue #3549, part of #3548. Nothing stops one module from querying the tables
of another module. When a monolith splits into services, these queries break
at run time. No wire contract shows them.

The architecture graph (#1747) already has an edge from each route, job and
repository to each table that it reaches, with access and evidence. The graph
reads one item's own tokens. It cannot follow a call into a helper.

## Decision

- **Config.** Boundaries go in `boundaries.toml` at the project root, not in
  `autumn.toml`. The check is a build-time rule. The runtime does not read it.
- **Membership.** A node is in a boundary by its crate (`crates`) or by its
  module path (`modules`). A table is in a boundary by `owns`. All lists are
  globs. The most specific pattern wins. An equal tie between two boundaries
  is an error.
- **Graph rule.** An edge from a node in A to a table that B owns is a
  violation. An edge to a repository stands for the tables of that
  repository's models.
- **Exceptions.** `may_read` allows a read edge. `may_write` allows every edge.
  The issue names only `may_read`. A write across a boundary is the worse
  case for a split, so it needs its own exception.
- **Crate rule.** A crate in A must not depend on a crate that holds B's
  schema. The check reads normal dependencies from `cargo metadata`. It
  follows crates in no boundary and stops at a crate in a boundary. The schema
  crates are the crates of B's models, or `schema_crates`. A `may_read` or
  `may_write` entry for a table of B allows the dependency.
- **Plugins.** A crate declares its tables in
  `[package.metadata.autumn.boundary] owns`. The check reads it from
  `cargo metadata`. A table that `boundaries.toml` already owns stays there.
  The graph rule sees a plugin table only when it has a `#[model]`.
  `plugin_migrations` gives no table names at run time, so it is not used.
- **Ratchet.** A JSON baseline holds the keys of known violations. A key has
  the access, but no evidence, line number or crate version. A new violation
  fails. A baseline entry that no longer occurs also fails, so a fixed entry
  cannot stay. An entry of a rule that did not run is not fixed.
- **Where.** The rules are in `autumn_web::graph::boundaries` with no IO. The
  CLI reads the files. `autumn graph check-boundaries` builds the app.
  `autumn doctor` reads a committed graph and runs `cargo metadata --offline`.
  A rule that cannot run is a warning, so `--strict` fails.

## Consequences

- An existing app can adopt the check on day one with a baseline.
- The check is as complete as the graph. A table with no `#[model]` has no
  node, so a query on it makes no edge. The guide states this limit.
- A route's access comes from its HTTP method. A `POST` route that reads
  another boundary's table needs `may_write`.

## Alternatives

- **Capture SQL at run time** (the `ledger-capture` path). This sees every
  statement, but needs a database and a test run. It can be a later change.
- **Keep the config in `autumn.toml`.** The runtime would carry keys it never
  uses, and the config schema gate would need to know them.
