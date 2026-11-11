# Table Ownership Boundaries

A boundary owns tables. Code in one boundary must not query a table that
another boundary owns. In a monolith, such a query works. When the modules
become separate services, the query fails at run time. A wire contract does
not show it. `autumn graph check-boundaries` finds these queries while the
app is still one service.

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
crates = ["crm-core*"]

[boundaries.marketing]
owns = ["campaigns", "ad_audiences", "attribution_*"]
crates = ["crm-marketing", "crm-core-ad*", "crm-core-attribution*"]
may_read = []
```

Each list holds glob patterns. `*` matches any run of characters.

| Key | What it holds |
|-----|---------------|
| `owns` | The tables of the boundary. Case does not matter. |
| `crates` | The crates and plugins in the boundary. `-` and `_` are equal. A package covers its library and its binaries. Their names can differ from the package name. |
| `modules` | Module paths in the boundary. `app::billing` also matches `app::billing::routes`. |
| `may_read` | Tables of other boundaries that this boundary may read. |
| `may_write` | Tables of other boundaries that this boundary may read and write. |
| `schema_crates` | The crates that hold the schema of the boundary. See [the crate rule](#the-crate-rule). |
| `schema` | The Postgres schema of the owned tables. See [Database Boundaries](database-boundaries.md). |
| `role` | The Postgres role of the boundary. See [Database Boundaries](database-boundaries.md). |

At the top level, `baseline` names the [baseline file](#adopt-the-check-with-a-baseline).
`graph` names a committed graph for `autumn doctor`. Both paths are relative to
`boundaries.toml`.

When two patterns match one name, the more specific pattern wins. Specific
means more characters that are not `*`. In the example above, `crm-core-ad*`
wins over `crm-core*` for the crate `crm-core-ad-sync`. When two boundaries
match one name equally, the check stops with an error. Make one pattern more
specific.

A node is in the boundary of the name at the start of its module path. That
name is a package, a library or a binary. When no pattern matches it, a
binary name gives the package of that binary. A binary with the name of a
library does not take the nodes of that library.

A node or a table in no boundary is not checked. Shared glue, for example the
`main.rs` of a package in no boundary, can stay outside all boundaries.

### A plugin declares its own tables

A plugin crate can declare the tables it owns in its `Cargo.toml`:

```toml
# Cargo.toml of the plugin
[package.metadata.autumn.boundary]
owns = ["admin_audit_log"]
```

The check reads this from `cargo metadata`. The tables join a boundary:

* A table that `boundaries.toml` already owns stays in that boundary.
* When a boundary in `boundaries.toml` lists the plugin crate, the tables join
  that boundary.
* Else the plugin gets a boundary of its own name, with its crate in it.

Code in another boundary that queries `admin_audit_log` is then a violation.
The graph sees the table only when the plugin declares a `#[model]` for it.

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
read only. `may_write` allows a read and a write.

* A `GET`, `HEAD`, `OPTIONS` or `TRACE` route reads. A route with another
  method writes.
* A job writes when its body has a mutating token, for example `UPDATE` or
  `.insert`.
* A repository reads and writes its own table.

A `POST` route that only reads a table of another boundary needs `may_write`,
or a safe method.

Each violation names both boundaries, the node, the table and the evidence of
the edge (`declaration`, `signature` or `body`, and the symbol).

## The crate rule

The graph reads one item's own tokens. It cannot follow a call into a helper
function. A helper in another crate can query a table, and the graph does not
see it. The crate rule finds this case.

A crate in boundary A must not depend on a crate that holds the schema of
boundary B. The check reads the dependencies from `cargo metadata`. It follows
dependencies through crates in no boundary, for example a shared helper crate.
It stops at a crate in a boundary, because the check examines that crate on its
own. A crate whose name a `modules` pattern matches also stops the search. Dev
and build dependencies do not ship, so the check ignores them. A package
matches by its name and its library name. A dependent links the library
only, so the crate rule does not use binary names.

The schema crates of a boundary are the crates that declare the models of its
tables. To name them yourself, set `schema_crates`. A `may_read` or
`may_write` entry for a table of B also allows the dependency on B's schema
crate, because the exception needs it.

* An entry with no `*` is a table name. It works with no graph.
* An entry with a `*` matches the tables of the models in the graph only.
  With `owns = ["*"]` on core, `may_read = ["campaign*"]` allows the schema
  crate of `campaigns` only, not the schema crate of core.
* With no graph, an entry with a `*` matches no table, and the check gives a
  warning. Write the table name, or check a graph.

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

The last rule is the ratchet. A fixed entry cannot stay in the baseline. If it
stays, a later change can bring the same violation back, and the check does
not see it.

`--update-baseline` writes all current violations. It can also add entries.
Read the diff of the baseline file in review. Each added entry is a new
cross-boundary query that someone accepts.

For the graph rule, the key of an entry is the boundary, the owner, the node
id, the table and the access. For the crate rule, it is the boundary, the
owner, the crate and the schema crate. The key holds no evidence, no line
number and no crate version. A refactor that keeps the query keeps the key. A
node that reads a table and then starts to write it gets a new key, so the
write fails.

When a rule cannot run, its baseline entries are not fixed. For example,
doctor with no graph does not report the graph entries as fixed.

## Run it

```console
autumn graph check-boundaries                      # build the app, check it
autumn graph check-boundaries --graph architecture-graph.json  # check a committed graph
autumn graph check-boundaries --config crm/boundaries.toml     # another config file
autumn graph check-boundaries --baseline other-baseline.json   # another baseline
autumn graph check-boundaries --update-baseline    # write the current violations
autumn graph check-boundaries --json               # new, known and fixed, as JSON
```

The check builds the app and reads its graph, as `autumn graph show` does. It
does not read the `graph` key; give `--graph` to check a committed file. Use
`--release` in CI, for the same reason as [the graph's drift gate](architecture-graph.md#keeping-it-honest).

The check runs `cargo metadata` in the current directory, with the same
feature flags as the build. Run it from the directory where you build the app.
`--baseline` is relative to the current directory. The `baseline` key is
relative to `boundaries.toml`.

The check exits 1 on a new violation, a fixed baseline entry, a bad config, or
a `cargo metadata` failure. Else it exits 0. `--update-baseline` always exits
0 when it writes the file. It cannot be used with `--json`.

### In `autumn doctor`

`autumn doctor` runs the check when `boundaries.toml` is in the current
directory. Doctor does not build the app:

* It reads the graph that the `graph` key names. Keep that file current with
  `autumn graph show --check architecture-graph.json`.
* It runs `cargo metadata --offline` with a time limit.

These results are warnings, so `autumn doctor --strict` fails on them:

* a new violation;
* a fixed baseline entry;
* a rule that cannot run, for example when `graph` is not set;
* a baseline file that cannot be read.

A bad `boundaries.toml`, or a name that two boundaries match equally, fails
doctor also without `--strict`. Doctor shows only the count of pattern
warnings. Run `autumn graph check-boundaries` to read them.

## Limits

The check is as complete as the graph. The [graph's limits](architecture-graph.md#what-the-derivation-cannot-see)
apply. These matter most here:

* **A table with no `#[model]` has no node.** The graph links a name to a
  table through a model. A query on a table with no model makes no edge.
* **A `use … as …` rename is not followed.** Write the table module or the
  model in the item.
* **SQL built at run time is not seen.** Only string literals are read. A
  [database role per boundary](database-boundaries.md) sees it.
* **A helper in the same crate is not followed.** The crate rule covers a
  helper in another crate only.

## Related

* [The Architecture Graph](architecture-graph.md) — the graph this check reads.
* [Database Boundaries](database-boundaries.md) — a role per boundary, so the
  database refuses the queries that this check cannot see.
* [Table width](table-width.md) — another schema rule that CI checks.
