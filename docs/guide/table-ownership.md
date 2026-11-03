# Table Ownership Boundaries

A boundary owns tables. Code in one boundary must not query a table that
another boundary owns. In a monolith, a cross-boundary query is a style
problem. When the modules become services, it breaks at run time, and no wire
contract shows it. `autumn graph check-boundaries` finds these queries before
the split.

```console
$ autumn graph check-boundaries
✗ 2 new boundary violation(s):
  marketing -> core: route GET /report reads table `contacts`
      node: route:crm_marketing::routes::report
      edge: body, symbol `contacts`
  marketing -> core: crate crm-marketing depends on crm-core, which holds the schema of `core`
      path: crm-marketing -> crm-core
```

## Declare the boundaries

Put a `boundaries.toml` file in the project root:

```toml
# boundaries.toml
baseline = "boundaries-baseline.json"
graph = "architecture-graph.json"

[boundaries.core]
owns = ["contacts", "companies", "deals", "activities"]
crates = ["crm-core-*"]

[boundaries.marketing]
owns = ["campaigns", "ad_audiences", "attribution_*"]
crates = ["crm-core-ad-*", "crm-core-attribution*"]
may_read = []
```

Each list holds glob patterns. `*` matches any run of characters.

| Key | What it holds |
|-----|---------------|
| `owns` | The tables of the boundary. Case does not matter. |
| `crates` | The crates and plugins in the boundary. `-` and `_` are equal. |
| `modules` | Module paths in the boundary. `app::billing` also matches `app::billing::routes`. |
| `may_read` | Tables of other boundaries that this boundary may read. |
| `may_write` | Tables of other boundaries that this boundary may read and write. |
| `schema_crates` | The crates that hold the schema of the boundary. See [the crate rule](#the-crate-rule). |

At the top level, `baseline` names the [baseline file](#adopt-the-check-with-a-baseline).
`graph` names a committed graph for `autumn doctor`. Both paths are relative to
`boundaries.toml`.

When two patterns match one name, the more specific pattern wins. Specific
means more characters that are not `*`. In the example above, `crm-core-ad-*`
wins over `crm-core-*` for the crate `crm-core-ad-sync`. When two boundaries
match one name equally, the check stops with an error. Make one pattern more
specific.

A node or a table in no boundary is not checked. Shared glue, for example
`main.rs`, can stay outside all boundaries.

### A plugin declares its own tables

A plugin crate can declare the tables it owns in its `Cargo.toml`:

```toml
# Cargo.toml of the plugin
[package.metadata.autumn.boundary]
owns = ["admin_audit_log"]
```

The check reads this from `cargo metadata`. The plugin gets a boundary of its
own name, with its crate in it. App code that queries `admin_audit_log` is then
a violation.

## The graph rule

The check reads the [architecture graph](architecture-graph.md). The graph has
an edge from each route, job, scheduled task and repository to each table that
it reaches. A violation is an edge from a node in boundary A to a table that
boundary B owns, when A does not list that table as an exception.

The graph finds a table through:

* a model or a `diesel` table module in the item, for example `contacts::table`;
* a table name in a raw-SQL string literal, which includes a `JOIN`;
* a repository in the item's signature. The report names the repository with `via`.

The access of the edge decides which exception applies. `may_read` allows a
read only. `may_write` allows a read and a write. The access of a route comes
from its HTTP method: `GET` reads, `POST` writes. A `POST` route that only
reads a table of another boundary needs `may_write` or a change of method.

Each violation names both boundaries, the node, the table and the evidence of
the edge (`declaration`, `signature` or `body`, and the symbol).

## The crate rule

The graph reads one item's own tokens. It cannot follow a call into a helper
function. A helper in another crate can query a table, and the graph does not
see it. The crate rule closes this gap.

A crate in boundary A must not depend on a crate that holds the schema of
boundary B. The check reads the dependencies from `cargo metadata`. It follows
dependencies through crates in no boundary, for example a shared helper crate.
It stops at a crate in a boundary, because the check examines that crate on its
own. Dev and build dependencies do not ship, so the check ignores them.

The schema crates of a boundary are the crates that declare the models of its
tables. To name them yourself, set `schema_crates`. A `may_read` or
`may_write` entry for a table of B also allows the dependency on B's schema
crate, because the exception needs it.

The crate rule only applies to boundaries with `crates`. A single-crate app
with `modules` boundaries uses the graph rule only.

## Draw the first boundary in an existing app

Start small. Pick one part of the app that you want to extract first.

1. Commit the graph. Run `autumn graph show --manifest architecture-graph.json`.
2. Find the tables of that part. Run `autumn graph touches <table>` for each one.
3. Write `boundaries.toml` with one boundary for that part and one for the
   rest. Use `modules` in a single-crate app.
4. Run `autumn graph check-boundaries`. Read the warnings. A pattern that
   matches nothing is often a typo.
5. Write the baseline: `autumn graph check-boundaries --update-baseline`.
6. Commit `boundaries.toml` and `boundaries-baseline.json`.
7. Add the check to CI. A new violation now fails the build.

```yaml
- run: autumn graph check-boundaries --release
```

## Adopt the check with a baseline

An existing app has violations on the first day. The baseline holds them, so
the app can adopt the check without a large refactor.

* A violation that is not in the baseline is **new**. It fails.
* A violation that is in the baseline is **known**. It passes.
* A baseline entry that no longer occurs is **fixed**. It fails until you
  remove it with `--update-baseline`.

The last rule is the ratchet. A fixed violation must leave the baseline. If it
stays, a later change can bring the same violation back, and the check does
not see it. So the baseline can only get smaller.

`--update-baseline` writes all current violations. It can also add entries.
Read the diff of the baseline file in review. Each added entry is a new
cross-boundary query that someone accepts.

The key of an entry is the boundary, the owner, the node id and the table. For
the crate rule, it is the boundary, the owner, the crate and the schema crate.
The key does not hold the evidence or a line number, so a refactor that keeps
the query keeps the key.

## Run it

```console
autumn graph check-boundaries                      # build the app, check it
autumn graph check-boundaries --graph architecture-graph.json  # check a committed graph
autumn graph check-boundaries --update-baseline    # write the current violations
autumn graph check-boundaries --json               # new, known and fixed, as JSON
```

The check builds the app and reads its graph, as `autumn graph show` does. Use
`--release` in CI, for the same reason as [the graph's drift gate](architecture-graph.md#keeping-it-honest).

`autumn doctor` runs the check when `boundaries.toml` exists. Doctor does not
build the app, so it reads the graph that `graph` names. Keep that file current
with `autumn graph show --check architecture-graph.json`. Doctor runs
`cargo metadata --offline`. A new violation, a fixed baseline entry, or a rule
that cannot run is a warning, so `autumn doctor --strict` fails on it.

## Limits

The check is as complete as the graph. The [graph's limits](architecture-graph.md#what-the-derivation-cannot-see)
apply. These matter most here:

* **A table with no `#[model]` has no node.** The graph links a name to a
  table through a model. A query on a table with no model makes no edge.
* **A `use … as …` rename is not followed.** Write the table module or the
  model in the item.
* **SQL built at run time is not seen.** Only string literals are read.
* **A helper in the same crate is not followed.** The crate rule covers a
  helper in another crate only.

## Related

* [The Architecture Graph](architecture-graph.md) — the graph this check reads.
* [Table width](table-width.md) — another schema rule that CI checks.
