# Autumn Plugins

Autumn integrations are packaged as **plugins**: small types that implement
[`autumn_web::Plugin`] and wire themselves into an `AppBuilder` with a single
`build(self, app)` call. Users compose plugins with `.plugin(...)` or the
tuple-taking `.plugins((...))`, and each plugin's `build` runs exactly once.

```rust
use autumn_web::app::AppBuilder;
use autumn_web::plugin::Plugin;

struct LiveFeedPlugin;

impl Plugin for LiveFeedPlugin {
    fn build(self, app: AppBuilder) -> AppBuilder {
        app.on_startup(|state| async move {
            tracing::info!(profile = state.profile(), "live feed started");
            Ok(())
        })
    }
}

autumn_web::app()
    .routes(routes![...])
    .plugin(LiveFeedPlugin)
    .run()
    .await;
```

## Installing a plugin

One command adds the dependency, mounts the plugin in your `autumn_web::app()`
builder chain, and prints whatever configuration the plugin still needs:

```bash
autumn plugin list                      # what can I install, at what version?
autumn plugin add autumn-admin-plugin   # dependency + mount + next steps
```

`autumn plugin list` shows every first-party plugin with the version compatible
with your app's `autumn-web`, plus community crates it finds on crates.io under
the `autumn-plugin-` naming convention below. Add `--json` for machine-readable
output, or `--offline` to skip the crates.io lookup.

`autumn plugin add` is safe to re-run: a second `add` of the same plugin
reports that it is already installed and changes nothing. It refuses — before
touching any file — to install a plugin whose supported `autumn-web` range
excludes your app's version, naming both versions. Pass `--dry-run` to see the
edits without applying them.

### Versions

First-party plugins are released in lockstep with `autumn-web` and with the
CLI, so the version `autumn plugin add` installs is the CLI's own. That makes
the version gate a statement about your toolchain: if your app is on an older
`autumn-web`, the listing marks each first-party plugin `[needs autumn-web
<series>]` and `add` refuses rather than writing a dependency that will not
resolve. Install the matching CLI for that series
(`cargo install autumn-cli --version <series>`), or bring the app forward with
`autumn upgrade`.

The command does **not** add feature flags to your `autumn-web` dependency.
Each plugin crate already depends on `autumn-web` with the features it needs
(`autumn-storage-s3` on `storage`, `autumn-cache-redis` on `redis`,
`autumn-admin-plugin` on `db`/`maud`/`htmx`/`flash`/`multipart`), and Cargo
unifies features across the graph — so the mount compiles without touching your
manifest beyond the one dependency line. The manual path below spells the
features out because a hand-written install may want them stated explicitly.

### What an install brings with it

`plugin add` and `new --with` write **code only** — a dependency line and a
mount. Neither runs a migration or creates a table. What a plugin does to your
database is its own business, and it happens later:

| Plugin | Database footprint | When it appears |
|---|---|---|
| `autumn-media-plugin` | `media_rooms`, `media_room_participants` | its `migrations/20260720000000_media_rooms` — **you** apply it, and only if you set `[media] room_store_backend = "db"` |
| `autumn-search` | `autumn_search_documents`, `autumn_search_deletes` | created at runtime by the Postgres engine (`CREATE TABLE IF NOT EXISTS`) the first time it starts |
| `autumn-admin-plugin` | none of its own | reads framework-owned tables that `autumn-web`'s own migrations create |
| `autumn-cache-redis` | none | entries live in Redis |
| `autumn-storage-s3` | none | blobs live in the bucket |

That table is the same list `autumn plugin remove` prints back to you, and it
is the reason removal leaves the database alone by default — see
[Removing a plugin](#removing-a-plugin).

### Community plugins

A crate following the third-party `autumn-plugin-<name>` convention gets its
dependency written for you, but **not** its mount: nothing outside that crate
can verify it really exposes `<Name>Plugin`, and a wrong guess would leave your
app not compiling. The command prints the convention-derived
`.plugin(...)` line to paste into your builder chain — check the crate's README
first.

### The manual path

`autumn plugin add` never leaves an app in a non-compiling state. If it cannot
find your `autumn_web::app()` builder chain — a heavily customized `main.rs`, or
a one-line chain with nowhere to splice a call — it changes **nothing** and
prints the exact dependency line and mount snippet to apply by hand instead.
That is also the path to follow when you would rather wire a plugin yourself:

```toml
# Cargo.toml
[dependencies]
autumn-admin-plugin = "0.7.0"
```

```rust,ignore
autumn_web::app()
    .plugin(autumn_admin_plugin::AdminPlugin::new())
    .run()
    .await;
```

## Removing a plugin

Installing a plugin is machine-applied, so removing one can be machine-reversed
— the lifecycle runs in both directions:

```bash
autumn plugin remove autumn-admin-plugin            # unwire dependency + mount
autumn plugin remove autumn-media-plugin --dry-run  # show every consequence first
```

`remove` deletes the `[dependencies]` line and the `.plugin(...)` /
`.with_blob_store(...)` call — including the ``// added by `autumn plugin add` ``
marker above it — and leaves everything else byte-identical. An app that was
installed with `autumn plugin add` passes `cargo check` immediately afterwards.

Like `add`, it is safe to re-run: removing a plugin that is not installed
reports that and changes nothing.

### What it refuses to do

`remove` never leaves an app that does not compile, which means it declines
three things rather than guessing:

- **A builder chain it cannot read.** A plugin built into a variable
  (`let panel = AdminPlugin::new(); … .plugin(panel)`), or a mount sharing its
  line with other builder calls, cannot be excised by deleting lines. Nothing
  is changed — not even the dependency — and the exact lines to delete are
  printed instead. (Exit code `2`, the same "apply this by hand" signal `add`
  uses.)
- **A dependency the app still uses.** If any file that a Cargo target is built
  from still names the crate after the mount comes out, the dependency stays and
  the report says which file kept it. That covers `src/`, `tests/`, `benches/`
  and `examples/`, the build script, and any target the manifest gives an
  explicit `path` to (`[[bin]] path = "cmd/server.rs"` and its sibling
  modules). Delete that usage and re-run.
- **A community mount.** `add` never writes one, so `remove` never deletes one.
  The dependency comes back out once nothing references the crate.

### Partially wired plugins

A manual install (the path below) leaves a dependency with no mount, or a mount
with no dependency. `remove` unwires whichever half it finds and names the half
it did not — the app still ends up clean.

### Data: the default is always "leave it"

**`plugin remove` never touches the database.** Unwiring code is reversible;
dropping data is not, so the two are never bundled. When a plugin declares
migrations or owns tables, the report lists them and states that they are still
there:

```
The database was not touched. autumn-media-plugin owns the following, and it is
all still there:
  migration  20260720000000_media_rooms
  table      media_room_participants
  table      media_rooms
```

To revert those migrations and drop those tables, ask for it explicitly:

```bash
autumn plugin remove autumn-media-plugin --drop-data          # asks first
autumn plugin remove autumn-media-plugin --drop-data --yes    # non-interactive
```

`--drop-data` prints the exact statements it will run, then asks for
confirmation (`--yes` answers for you; a non-interactive stdin without `--yes`
is a refusal, never an assumed yes). It drives Postgres; with any other
backend, or no database configured, it prints the statements for you to run
yourself rather than failing after the prompt. It works from the plugin's
**declared** migration list, because `__diesel_schema_migrations` has no source
column — a migration applied before this feature shipped cannot be attributed
to a plugin any other way.

### Exit codes

| Code | Meaning |
|---|---|
| `0` | Removed, or nothing to do |
| `1` | Refused: unknown plugin, not an Autumn project, or the `--drop-data` step failed |
| `2` | Nothing was changed automatically — apply the printed edits by hand. Also `--drop-data`'s answer when it printed the statements instead of running them (no database configured, or a non-Postgres backend), and when a dependency is declared in a shape the command will not rewrite |
| `3` | `--dry-run` only: a real run **would** change something |

`--dry-run` prints every file edit and every data consequence and writes
nothing, so `3` versus `0` answers "is there anything left to remove?" without
parsing prose.

## Scaffolding an app with plugins

`autumn new --with <plugin>` wires a plugin on day zero, so nothing has to be
retrofitted:

```bash
autumn new my-app --with autumn-admin-plugin
autumn new my-app --with autumn-admin-plugin --with autumn-search
```

`--with` is repeatable and takes the same names as `autumn plugin add`. Every
name is resolved and version-checked **before the scaffold writes a byte**: an
unknown or incompatible plugin leaves no half-built project behind. Repeating a
name is a typo, not an error. The generated app compiles on the first try.

Community `autumn-plugin-<name>` crates work here too, with the same
dependency-only rule: the dependency is written, the mount is printed for you to
paste.

`--with` composes with `--starter`. One difference: a starter brings its own
`Cargo.toml`, so its `autumn-web` pin is not knowable until the starter has been
fetched. Names are still resolved before anything is written, but a starter
pinned to a different series gets its compatibility answer afterwards — the app
is scaffolded, the plugin is reported as not wired, and the command exits `2`
so a script does not read it as a complete install.

Scaffolding wires code, not data: exactly like `plugin add`, `--with` runs no
migration and creates no table. See
[What an install brings with it](#what-an-install-brings-with-it) for what each
plugin puts in the database, and when.

## Finding residue

`autumn doctor` reports orphaned plugin wiring under its existing
`--json`/`--strict` contract, as the `plugin_residue` check:

| Finding | Status |
|---|---|
| Dependency declared, never mounted | warn |
| Mounted, but not declared in `[dependencies]` (does not compile) | fail |
| Plugin gone, but migrations it declares are still applied | warn |

The migration finding is best effort: it needs a configured database and the
`diesel` CLI to read the history. Without either, the two static findings still
run.

## First-party plugin crates

| Crate | What it adds | Guide |
|---|---|---|
| `autumn-admin-plugin` | Admin UI and API-token administration | [Admin](./guide/admin.md) |
| `autumn-media-plugin` | Live-streaming media (broadcast + rooms) | — |
| `autumn-storage-s3` | S3-backed object storage | [Storage](./guide/storage.md) |
| `autumn-cache-redis` | Redis-backed shared cache | [Cache stampede](./guide/cache-stampede.md) |
| `autumn-search` | Keyword **and** vector search with lifecycle-synced indexes | [Search](./guide/search.md) |

## Naming conventions

| Kind | Crate name | Struct name |
|------|------------|-------------|
| First-party, adds a *subsystem* | `autumn-<name>-plugin` | `<Name>Plugin` |
| First-party, *implements a seam* for a named technology | `autumn-<subsystem>-<technology>` or `autumn-<name>` | `<Name>Plugin` |
| Autumn companion (separate release train) | `autumn-<name>` or `autumn-<name>-plugin` | `<Name>Plugin` |
| Third-party (lives on crates.io) | `autumn-plugin-<name>` | `<Name>Plugin` |

Third-party crates keep the `autumn-plugin-` prefix so the ecosystem
is easy to search on crates.io. First-party crates reverse the order so
they cluster with the crate they extend.

The second row is what `autumn-storage-s3`, `autumn-cache-redis`, and
`autumn-search` follow: the crate name says which subsystem it provides rather
than repeating `-plugin`, because the interesting part of the name is the seam,
not the fact that it happens to ship as a plugin.

Companion crates can live outside this repository when their dependency graph
points back at `autumn-web`. Autumn Harvest is the main example: it provides
durable workflows and may expose an Autumn adapter/plugin, but `autumn-web`
does not compile examples against Harvest. That keeps web releases independent
while still giving users an obvious path to workflow orchestration.

Every plugin crate should expose its `<Name>Plugin` type at the crate
root along with a `::new()` constructor and `#[must_use]` fluent
configuration methods.

## Authoring a plugin

```rust
use autumn_web::app::AppBuilder;
use autumn_web::plugin::Plugin;

pub struct HelloPlugin {
    greeting: String,
}

impl HelloPlugin {
    #[must_use]
    pub fn new() -> Self {
        Self { greeting: "hello".to_owned() }
    }

    #[must_use]
    pub fn greeting(mut self, greeting: impl Into<String>) -> Self {
        self.greeting = greeting.into();
        self
    }
}

impl Default for HelloPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for HelloPlugin {
    fn build(self, app: AppBuilder) -> AppBuilder {
        let greeting = self.greeting;
        app.on_startup(move |_state| {
            let greeting = greeting.clone();
            async move {
                tracing::info!(%greeting, "hello plugin started");
                Ok(())
            }
        })
    }
}
```

Inside `build`, you have the full `AppBuilder` surface:
`on_startup`, `on_shutdown`, `nest`, `with_extension` / `extension`,
`migrations` (with the `db` feature), `routes`, and so on. Prefer
chaining the existing builder methods over reinventing infrastructure.

## Duplicate registration

Two plugins that share the same [`Plugin::name`] cannot both apply to
the same builder. The default name is `std::any::type_name::<Self>()`,
so a second instance of the same plugin type is skipped with a
`tracing::warn!`. Override `name` only if a plugin is genuinely
designed to be registered more than once (rare — most plugins should
accept a `Vec`-shaped input instead).

`name` returns [`Cow<'static, str>`], so plugins can compute a unique
label from runtime configuration without leaking memory:

```rust
use std::borrow::Cow;

impl Plugin for ShardedPlugin {
    fn name(&self) -> Cow<'static, str> {
        Cow::Owned(format!("sharded-plugin:{}", self.shard))
    }
    // ...
}
```

[`Cow<'static, str>`]: https://doc.rust-lang.org/std/borrow/enum.Cow.html

## Object safety

`Plugin::build` consumes `self`, so `Plugin` is **not** object-safe.
This is deliberate: keeping `self` by value lets config methods stay
zero-overhead (no `Box<dyn Fn>` or dynamic dispatch on every call) and
makes the plugin's builder type signature match Autumn's own
consuming-self builder style. Users who need dynamic plugin collections
can hide types behind an explicit enum or build their own trait object.

## Cooperative plugins

A plugin may want to behave differently when another plugin is present
(for example, skipping its own migrations when a sibling already
registered them). Check with [`AppBuilder::has_plugin`]:

```rust
impl Plugin for MyTelemetryPlugin {
    fn build(self, app: AppBuilder) -> AppBuilder {
        if app.has_plugin(std::any::type_name::<OtherTelemetryPlugin>()) {
            tracing::info!("other telemetry plugin already registered; noop");
            return app;
        }
        app.with_extension(self.exporter)
    }
}
```

[`autumn_web::Plugin`]: https://docs.rs/autumn-web/latest/autumn_web/plugin/trait.Plugin.html
[`Plugin::name`]: https://docs.rs/autumn-web/latest/autumn_web/plugin/trait.Plugin.html#method.name
[`AppBuilder::has_plugin`]: https://docs.rs/autumn-web/latest/autumn_web/app/struct.AppBuilder.html#method.has_plugin

## Exposing plugin routes as MCP tools

Typed routes a plugin registers via `AppBuilder::routes()`/`scoped()` flow
into the host's route registry, so they can be projected into MCP tools like
any user route. The convention is to make this **host opt-in** through your
fluent config, flipping the routes with the chainable `Route::mcp()` toggle
inside `Plugin::build`, so the host writes:

```rust
autumn_web::app()
    .plugin(HarvestPlugin::new().expose_mcp())
    .mount_mcp("/mcp")
```

The full `expose_mcp()` recipe (and the `Route::mcp_exclude()` /
`Route::mcp_stream()` toggles that mirror the other `#[api_doc]` forms) lives
in the [MCP guide, §10 "Plugins and route-level opt-in"](guide/mcp.md); the
compile-checked canonical example is on
[`Route::mcp`](https://docs.rs/autumn-web/latest/autumn_web/struct.Route.html#method.mcp).
The flags are plain metadata — inert unless the host enables the `mcp`
feature and calls `mount_mcp` — so setting them requires no feature gate in
your plugin crate. Raw routers mounted via `nest()`/`merge()` are opaque to
the registry and cannot be derived into tools; register typed routes for any
endpoint that should be MCP-exposable.

---

## The plugin API contract

Autumn declares which plugin-facing APIs are **stable** and which are
**experimental**, so a plugin author knows what they may build on and what may
move under them. The declaration is machine-readable — it lives in
[`autumn_web::plugin_contract::PLUGIN_SURFACES`](https://docs.rs/autumn-web/latest/autumn_web/plugin_contract/constant.PLUGIN_SURFACES.html)
— and the table below mirrors it, entry for entry.
`scripts/check-plugin-surface.sh` fails CI when the two disagree, so this page
cannot quietly go stale.

### The tiers, and what each promises

| Tier | Promise | How a change reaches you |
|------|---------|--------------------------|
| **stable** | Covered by the [Stability Policy](../STABILITY.md). Below `1.0` a break requires a minor bump, a [migration guide](migrations/README.md), and a filled **Plugin authors** section in it; from `1.0` on it requires a major bump and a deprecation cycle. | A compiler error with a named upgrade path, announced in the release's migration guide before you hit it. |
| **experimental** | May change in **any** release, including a patch. Declare your use of it with `PluginContract::uses_experimental` so `autumn plugin-check` reports it. | A compiler error you opted into. Not guaranteed a guide section. |

Anything **not** in the table is neither: it is ordinary `autumn-web` public
API, covered by the crate-wide SemVer promise in
[`STABILITY.md`](../STABILITY.md) but not singled out as part of the plugin
contract. The distinction matters because the plugin surface is the part CI
proves on every commit, by compiling a pinned reference plugin against the
framework (see [The framework-side gate](#the-framework-side-gate)).

### The declared surface

| API | Tier | Notes |
|-----|------|-------|
| `AppBuilder::config_section` | stable | Declare a plugin-owned top-level config root so `server.strict_config` treats it as known-and-opaque. |
| `AppBuilder::declare_plugin_routes` | stable | Make routes mounted through an opaque `nest`/`merge` router visible to `autumn routes` and the conformance harness. |
| `AppBuilder::error_pages` | stable | Replace the rendered error pages — a tier-1 subsystem seam (requires the `maud` feature). The plugin crate needs its own `maud` dependency: `html!` expands to absolute `::maud::` paths, which no re-export can satisfy. |
| `AppBuilder::merge` | stable | Merge a raw axum router at the root. Pair it with `declare_plugin_routes` so the routes stay attributable. |
| `AppBuilder::nest` | stable | Mount a raw axum router under a prefix. Pair it with `declare_plugin_routes` so the routes stay attributable. |
| `AppBuilder::on_shutdown` | stable | Register an async shutdown hook that runs during graceful drain. |
| `AppBuilder::on_startup` | stable | Register an async startup hook that runs once before the server binds. |
| `AppBuilder::plugin` | stable | Mount a plugin. Also the seam a cooperative plugin uses to mount a plugin of its own. |
| `AppBuilder::plugin_contracts` | stable | Read the contracts declared by the plugins mounted on a builder — what the route dump and `autumn plugin-check` are built on. |
| `AppBuilder::plugin_migrations` | stable | Contribute embedded database migrations tagged with the plugin's own name. Needs `reexports::diesel_migrations` in scope, because `embed_migrations!` expands to unqualified paths (requires the `db` feature). |
| `AppBuilder::plugins` | stable | Mount a tuple of up to eight plugins in declaration order. |
| `AppBuilder::routes` | stable | Register typed routes from `routes![]`. Plugin routes registered this way are attributed automatically. |
| `AppBuilder::with_config_loader` | stable | Replace the tier-1 configuration loader (e.g. a secrets-manager backend). |
| `AppBuilder::with_edge_kv` | experimental | Edge-capsule KV binding (requires the `edge` feature). The whole edge lane (issue #1790) may change in any release; the capsule wire protocol carries its own version field. |
| `AppBuilder::with_extension` | stable | Publish a typed value into application state for handlers to extract. |
| `AppBuilder::with_pool_provider` | stable | Replace the tier-1 database pool provider (requires the `db` feature). |
| `AppBuilder::with_session_store` | stable | Replace the tier-1 session store. |
| `AppBuilder::with_telemetry_provider` | stable | Replace the tier-1 telemetry provider. |
| `Plugin::build` | stable | The one required method: apply the plugin's wiring to the builder. Runs exactly once per app. |
| `Plugin::contract` | stable | Declare the `autumn-web` range this plugin supports and any experimental surface it uses. |
| `Plugin::name` | stable | Stable identifier used for duplicate-registration detection and route attribution. |
| `autumn_edge::host` | experimental | Reference edge-capsule host API. Experimental alongside the rest of the edge lane (issue #1790). |
| `db::Pool` | stable | The pool type `DatabasePoolProvider::create_pool` returns, re-exported so a plugin need not depend on `diesel-async` itself (requires the `db` feature). |
| `plugin_conformance` | stable | The library-level conformance harness plugin authors run in their own test suite. |
| `plugin_contract` | stable | This module: `PluginContract`, `PLUGIN_SURFACES`, `SurfaceTier`, `evaluate`, `lockstep_range`, and the `PLUGIN_CONTRACT_MARKER` dump protocol. |
| `route_listing::RouteInfo` | stable | The route manifest type the conformance harness and `autumn routes --format json` share. |

### Declaring what your plugin supports

A plugin declares the `autumn-web` range it works with by implementing
`Plugin::contract`:

```rust,ignore
use autumn_web::plugin_contract::PluginContract;

impl Plugin for MyPlugin {
    fn contract(&self) -> Option<PluginContract> {
        Some(
            PluginContract::new(env!("CARGO_PKG_NAME"))
                .plugin_version(env!("CARGO_PKG_VERSION"))
                .autumn_web("0.7"),
        )
    }

    fn build(self, app: AppBuilder) -> AppBuilder { /* ... */ app }
}
```

`autumn_web(...)` takes a Cargo version requirement: `"0.7"` for a single minor
series, `">=0.6, <0.9"` for a range, `"=0.7.1"` for an exact pin. Below `1.0`
every minor bump is breaking, so declare the minor series; from `1.0` on, the
major. Declare the range you have actually verified — a stale narrow literal is
the one thing that makes this fail on an app that would otherwise work.

A plugin released **in lockstep** with the framework (every first-party crate
in this repository) can derive the range instead of writing it, so a version
bump cannot leave it behind:

```rust,ignore
use autumn_web::plugin_contract::lockstep_range;

PluginContract::new(env!("CARGO_PKG_NAME"))
    .autumn_web(lockstep_range(env!("CARGO_PKG_VERSION")))
```

Do **not** reach for that if your crate versions independently: a third-party
plugin at its own `1.2.0` would derive `"1"`, which excludes every `0.x`
framework.

Reading `env!("CARGO_PKG_NAME")` and `env!("CARGO_PKG_VERSION")` rather than
literals keeps the contract from drifting away from the crate it describes.

**Two names, one flag.** A contract names your *crate*, while route attribution
keys on [`Plugin::name`](#authoring-a-plugin) — which defaults to
`std::any::type_name`, not the crate name. `autumn plugin-check --plugin-name`
takes one string, and it resolves against **either**, so neither choice leaves
your plugin unfindable. Overriding `name()` to your crate name anyway is worth
doing: it is what shows up in `autumn routes` output as `plugin:<name>`.

**Declaring nothing still compiles and runs.** `Plugin::contract` defaults to
`None`, and a plugin that returns `None` behaves at runtime exactly as it did
before the contract existed — it just gets no diagnostic when it is mounted
into a framework it was not written for.

It does **not** pass `autumn plugin-check`, though: the `plugin-contract` check
fails a plugin that declares no usable range. That is the author-facing gate
doing its job — "you have not said which framework versions you support" is the
finding — and the four lines above clear it. `autumn generate plugin`
scaffolds them for you.

### What an incompatible pairing looks like

`AppBuilder::plugin` evaluates the contract when the plugin is registered — at
application startup, before anything binds — and **panics** on a range that
excludes the framework in the build:

```text
plugin `autumn-plugin-example 0.6.2` supports autumn-web 0.6, but this application builds against autumn-web 0.7.0.
  → upgrade the plugin to a release built for autumn-web 0.7.0 (`cargo update -p autumn-plugin-example`), or
  → pin the framework the plugin supports: autumn-web = "0.6"
  → or, to boot anyway while you sort it out, set AUTUMN_PLUGIN_CONTRACT=warn
```

The last line is the escape hatch. Cargo has already proven the app and the
plugin link one `autumn-web` — otherwise there would be two copies and a
compile error — so a mismatch here means the plugin's *declared* range is
narrower than what actually resolved, usually a stale literal. That is the
plugin author's to fix, and an app author should not be stuck on a
non-booting deployment waiting for them: `AUTUMN_PLUGIN_CONTRACT=warn`
downgrades the panic to a `WARN` log carrying the same text.

Both versions and both remedies are in the message, which is the whole point:
the alternative is a subtly wrong runtime, or a compiler error several layers
removed from its cause.

A requirement string that cannot be parsed only *warns* at startup — it is the
plugin author's typo, and an application author cannot fix it — but
`autumn plugin-check` **fails** on it, which is where the author will see it.

### Depending on experimental surface

If your plugin uses an API declared `experimental` above, say so:

```rust,ignore
PluginContract::new(env!("CARGO_PKG_NAME"))
    .autumn_web("0.7")
    .uses_experimental("AppBuilder::with_edge_kv")
```

`autumn plugin-check` then reports it (it does not fail — building on
experimental surface is an informed choice):

```text
✓ [PASS] experimental-surface: depends on 1 experimental surface(s); these may change in any release
  → AppBuilder::with_edge_kv: Edge-capsule KV binding. The whole edge lane (issue #1790) may change in any release; …
```

Two things *do* fail the check, because both make the declaration meaningless:
a name that is not in the registry at all (a typo), and a name that is in the
registry at the **stable** tier (which overstates your exposure).

Pass `--deny-experimental` to turn the report into a gate in your own CI:

```bash
autumn plugin-check --plugin-name autumn-plugin-mine --deny-experimental
```

### The framework-side gate

The contract runs in both directions. On Autumn's side, the
`autumn-plugin-reference` crate is a real `Plugin` implementation that calls
**every** stable surface in the table above, built by the `plugin-contract` CI
job on every change to the framework. Removing, renaming, or re-signaturing a
stable plugin API breaks that build — in Autumn's CI, before it reaches yours.

A registry entry that no reference-plugin call site exercises fails the same
job, so the table cannot list a promise nothing checks.

When a release does change the stable plugin surface, its migration guide
carries a **Plugin authors** section saying what changed and what to do —
enforced by `scripts/check-plugin-surface.sh`, which fails a change to the
declared surface that does not update
[`docs/migrations/next.md`](migrations/next.md).

---

## Plugin conformance and publishing checklist

Before publishing a plugin crate to crates.io, run the Autumn conformance
flow to prove your plugin is safe to install in a real host app.

### 1. Run conformance against a minimal host app

Create a small example or test binary that installs your plugin, then run:

```bash
autumn plugin-check \
  --plugin-name autumn-myplugin-plugin \
  --prefix /my-prefix \
  --sensitive-route /my-prefix:"Role: myadmin required" \
  -p my-conformance-app
```

This checks:

| Check | What it verifies |
|-------|-----------------|
| `installability` | Binary compiles and route manifest is produced |
| `route-attribution` | Every plugin route carries `plugin:<your-name>` source |
| `route-prefix` | Every plugin route lives under the declared prefix |
| `route-collision` | No two routes share (method, path); names the conflicting handlers and sources |
| `sensitive-surfaces` | Routes with admin/debug/credential/operator/secret/metrics paths are declared with auth mechanisms |
| `duplicate-registration` | No plugin route appears more than once, which would indicate the plugin was installed twice |
| `plugin-contract` | The plugin declares a parseable `autumn-web` range via `Plugin::contract` |
| `experimental-surface` | Which experimental plugin API the plugin declares a dependency on |

Add `--format json` to produce a machine-readable report suitable for CI:

```bash
autumn plugin-check --plugin-name autumn-myplugin-plugin --prefix /my-prefix \
  --sensitive-route /my-prefix:"Role: myadmin required" \
  --format json | tee conformance-report.json
```

### 2. Write library-level conformance tests

For tighter integration, use `autumn_web::plugin_conformance` in your
test suite to verify conformance at `cargo test` time without a separate
binary step:

```rust
#[cfg(test)]
mod conformance_tests {
    use autumn_web::plugin::Plugin;
    use autumn_web::plugin_conformance::{ConformanceConfig, run_conformance};
    use autumn_web::route_listing::{RouteInfo, RouteSource};

    use crate::MyPlugin;

    #[test]
    fn plugin_passes_conformance() {
        // Simulate the routes your plugin contributes
        let routes = vec![
            RouteInfo {
                method: "GET".to_owned(),
                path: "/my-prefix".to_owned(),
                handler: "myplugin::index".to_owned(),
                source: RouteSource::Plugin("autumn-myplugin-plugin".to_owned()),
                middleware: vec![],
            },
        ];

        let config = ConformanceConfig::new("autumn-myplugin-plugin")
            .prefix("/my-prefix")
            .sensitive_route("/my-prefix", "Role: myadmin required")
            // Pass the plugin's own contract so the `experimental-surface`
            // check runs instead of being skipped. `Plugin` must be in scope
            // for the `contract()` call.
            .contract(
                MyPlugin::new()
                    .contract()
                    .expect("MyPlugin declares a contract"),
            );

        let report = run_conformance(&config, &routes);
        assert!(report.passed(), "conformance failed:\n{}", report.to_text_report());
    }
}
```

### 3. Publishing checklist

Work through this list before `cargo publish`:

- [ ] **Crate name** — follows the `autumn-<name>-plugin` (first-party) or
  `autumn-plugin-<name>` (third-party) convention
- [ ] **Install snippet** — README includes a one-line `.plugin(MyPlugin::new())`
  install example with the correct import path
- [ ] **Route prefix** — all plugin routes live under a documented prefix,
  or any root-level routes are explicitly explained in the README
- [ ] **Route manifest** — `autumn routes --format json` on a host app shows
  every plugin route with `"source": "plugin:<your-name>"`. If your plugin
  uses `AppBuilder::nest()` (whose routes are opaque to the listing), call
  `AppBuilder::declare_plugin_routes(routes)` alongside `nest()` to make
  those routes visible.
- [ ] **Compatibility contract** — `Plugin::contract` declares the supported
  `autumn-web` range, and any experimental surface the plugin uses is declared
  with `uses_experimental` (see [The plugin API contract](#the-plugin-api-contract))
- [ ] **Production exposure gates** — if the plugin mounts admin, debug,
  credential, operator, secret, or metrics surfaces, the README documents
  the auth/profile gating mechanism and conformance passes with
  `--sensitive-route PATH:DESCRIPTION`
- [ ] **SemVer expectations** — breaking changes to the `Plugin::build`
  signature or to any mounted route path bump the major version
- [ ] **Conformance report** — `autumn plugin-check` exits 0 and the
  CI log shows "All conformance checks passed"
- [ ] **Duplicate-registration contract** — installing the plugin twice
  is a no-op (second registration is skipped with a warning); document
  whether your plugin is designed to be registered more than once
- [ ] **Existing app compatibility** — downstream apps that only consume
  the plugin continue to compile and run unchanged after each release

### Reference example: `autumn-admin-plugin`

`autumn-admin-plugin` is the first-party reference for the conformance
workflow.  See `autumn-admin-plugin/src/lib.rs` for the library-level
conformance test that runs as part of `cargo test`.

To run the CLI conformance check against the admin plugin's example app:

```bash
autumn plugin-check \
  -p bookmarks \
  --plugin-name autumn-admin-plugin \
  --prefix /admin \
  --sensitive-route /admin:"Role: admin required via AdminPlugin::require_role"
```
