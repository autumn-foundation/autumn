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

## Naming conventions

| Kind | Crate name | Struct name |
|------|------------|-------------|
| First-party (lives in this repo) | `autumn-<name>-plugin` | `<Name>Plugin` |
| Autumn companion (separate release train) | `autumn-<name>` or `autumn-<name>-plugin` | `<Name>Plugin` |
| Third-party (lives on crates.io) | `autumn-plugin-<name>` | `<Name>Plugin` |

Third-party crates keep the `autumn-plugin-` prefix so the ecosystem
is easy to search on crates.io. First-party crates reverse the order so
they cluster with the crate they extend.

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
any user route (see the [MCP guide](guide/mcp.md)). The convention is to make
this **host opt-in** through your fluent config, flipping the routes with the
chainable `Route::mcp()` toggle at build time:

```rust
use autumn_web::Route;

pub struct HarvestPlugin {
    expose_mcp: bool,
}

impl HarvestPlugin {
    #[must_use]
    pub fn expose_mcp(mut self) -> Self {
        self.expose_mcp = true;
        self
    }
}

impl Plugin for HarvestPlugin {
    fn build(self, app: AppBuilder) -> AppBuilder {
        let mut rs = routes![list_runs, create_run];
        if self.expose_mcp {
            rs = rs.into_iter().map(Route::mcp).collect();
        }
        app.routes(rs)
    }
}
```

`Route::mcp_exclude()` and `Route::mcp_stream()` mirror the other
`#[api_doc]` forms. The flags are plain metadata — inert unless the host
enables the `mcp` feature and calls `mount_mcp` — so setting them requires no
feature gate in your plugin crate. Raw routers mounted via `nest()`/`merge()`
are opaque to the registry and cannot be derived into tools; register typed
routes for any endpoint that should be MCP-exposable.

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
    use autumn_web::plugin_conformance::{ConformanceConfig, run_conformance};
    use autumn_web::route_listing::{RouteInfo, RouteSource};

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
            .sensitive_route("/my-prefix", "Role: myadmin required");

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
