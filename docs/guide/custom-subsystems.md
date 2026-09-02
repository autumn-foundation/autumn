# Replacing Autumn subsystems with custom impls

This guide is the per-trait how-to for **tier-1** subsystem replacement. For
the bigger picture — when to reach for tier-1 vs tier-2 (`#[intercept]`)
vs tier-3 (`Plugin`) — see [`extensibility.md`](extensibility.md).

Each tier-1 subsystem in Autumn follows the same shape:

1. **A trait** in the subsystem's home module.
2. **A default impl** that wraps the framework's existing behaviour.
3. **A fluent builder method** on `AppBuilder` (`with_<subsystem>`) that
   replaces the default with your impl.

Replacement is opt-in. Apps that don't call `with_<subsystem>` see no
behaviour change.

---

## `ConfigLoader` — replace the TOML + env config layering

```rust,no_run
use autumn_web::config::{AutumnConfig, ConfigError, ConfigLoader};

pub struct JsonFileConfigLoader { path: std::path::PathBuf }

impl ConfigLoader for JsonFileConfigLoader {
    async fn load(&self) -> Result<AutumnConfig, ConfigError> {
        let bytes = std::fs::read(&self.path).map_err(ConfigError::Io)?;
        serde_json::from_slice(&bytes)
            .map_err(|e| ConfigError::Validation(e.to_string()))
    }
}

# use autumn_web::prelude::*;
#[autumn_web::main]
async fn main() {
    autumn_web::app()
        .with_config_loader(JsonFileConfigLoader { path: "config.json".into() })
        .run()
        .await;
}
```

The snippet above is the whole integration — there is no hidden setup step.
For the trait's full contract see
[`ConfigLoader`](https://docs.rs/autumn-web/latest/autumn_web/config/trait.ConfigLoader.html)
and [`with_config_loader`](https://docs.rs/autumn-web/latest/autumn_web/app/struct.AppBuilder.html#method.with_config_loader).

**When to reach for it:** AWS Secrets Manager, Vault, Consul, an HTTP fetch,
a JSON/YAML file, encrypted overlays, anything that's not five-layer TOML +
env vars. The trait's only contract is "produce a fully-resolved
`AutumnConfig` or a `ConfigError`" — the framework handles the rest of the
boot sequence the same way it would for the default loader.

---

## `DatabasePoolProvider` — replace the deadpool + diesel-async pool factory

```rust,no_run
use autumn_web::config::DatabaseConfig;
use autumn_web::db::{DatabasePoolProvider, PoolError};
use diesel_async::AsyncPgConnection;
use diesel_async::pooled_connection::deadpool::Pool;

pub struct MetricsPoolProvider;

impl DatabasePoolProvider for MetricsPoolProvider {
    async fn create_pool(
        &self,
        config: &DatabaseConfig,
    ) -> Result<Option<Pool<AsyncPgConnection>>, PoolError> {
        // Wrap the default factory's output with your own metrics, circuit
        // breaker, custom builder, etc. before returning.
        autumn_web::db::create_pool(config)
    }
}

# use autumn_web::prelude::*;
#[autumn_web::main]
async fn main() {
    autumn_web::app()
        .with_pool_provider(MetricsPoolProvider)
        .run()
        .await;
}
```

**When to reach for it:** custom metrics wrappers, circuit breakers, separate
pools per shard, sidecar connection lifecycle (warmup queries, custom probe
endpoints), or alternative builders that still produce
`Pool<AsyncPgConnection>`.

**What this trait does NOT abstract:** the pool *type*. The return is always
`Pool<AsyncPgConnection>`. Switching to a non-Postgres backend (MySQL, SQLite)
would require generic `Pool<C>` propagation through `Db`, `DbState`, and
`AppState` — a much larger refactor that's intentionally out of scope.

---

## `TelemetryProvider` — replace the tracing + OTLP initializer

```rust,no_run
use autumn_web::config::{LogConfig, TelemetryConfig};
use autumn_web::telemetry::{TelemetryGuard, TelemetryInitError, TelemetryProvider};

pub struct DatadogTelemetryProvider;

impl TelemetryProvider for DatadogTelemetryProvider {
    fn init(
        &self,
        _log: &LogConfig,
        _telemetry: &TelemetryConfig,
        _profile: Option<&str>,
    ) -> Result<TelemetryGuard, TelemetryInitError> {
        // Configure datadog-tracing here. Return a TelemetryGuard whose
        // Drop impl flushes your exporter.
        Ok(TelemetryGuard::disabled())
    }
}

# use autumn_web::prelude::*;
#[autumn_web::main]
async fn main() {
    autumn_web::app()
        .with_telemetry_provider(DatadogTelemetryProvider)
        .run()
        .await;
}
```

**When to reach for it:** Datadog tracer, Honeycomb beeline, Sentry
breadcrumb integration, custom JSON aggregator, or anything else that wants
control of the global tracing subscriber and exporter setup.

**Synchronous on purpose:** `init` mirrors the underlying
`tracing-subscriber` API. If your provider needs async setup (HTTP
discovery, registration with a control plane), do that work inside the
returned `TelemetryGuard`'s lifecycle hooks, or spin up an internal runtime
inside `init`.

---

## `SessionStore` — replace the memory/redis backend with anything else

```rust,no_run
use std::collections::HashMap;
use autumn_web::session::{SessionStore, SessionStoreError};

pub struct EncryptedCookieStore;

impl SessionStore for EncryptedCookieStore {
    async fn load(
        &self,
        _id: &str,
    ) -> Result<Option<HashMap<String, String>>, SessionStoreError> {
        // Decrypt cookie payload, deserialize, return.
        Ok(None)
    }

    async fn save(
        &self,
        _id: &str,
        _data: HashMap<String, String>,
    ) -> Result<(), SessionStoreError> {
        // Serialize, encrypt, set cookie via response handler.
        Ok(())
    }

    async fn destroy(&self, _id: &str) -> Result<(), SessionStoreError> {
        Ok(())
    }
}

# use autumn_web::prelude::*;
#[autumn_web::main]
async fn main() {
    autumn_web::app()
        .with_session_store(EncryptedCookieStore)
        .run()
        .await;
}
```

When you install a custom store, `build_session_layer` skips the
config-driven `memory` vs `redis` selection entirely — your store handles
all sessions for the app.

**When to reach for it:** database-backed sessions, encrypted cookie stores,
enterprise SSO bridges, multi-tenant session isolation, or anything else
that doesn't fit the built-in memory/Redis split.

---

## `ChannelsBackend` - replace the local/Redis realtime backend

```rust,no_run
use autumn_web::prelude::*;

#[autumn_web::main]
async fn main() {
    autumn_web::app()
        .with_channels_backend(LocalChannelsBackend::new(64))
        .routes(routes![/* ... */])
        .run()
        .await;
}
```

When you install a custom channels backend, AppState skips the config-driven
`in_process` vs `redis` selection. The backend owns publish, subscribe,
topic cleanup, and `/actuator/channels` metrics.

**When to reach for it:** NATS, Postgres `LISTEN/NOTIFY`, sharded Redis,
test harnesses, or application-specific fan-out rules that should still use
the same `AppState::channels()` and `AppState::broadcast()` API.

See [`realtime.md`](realtime.md) for the built-in Redis backend and htmx/SSE
helpers.

---

## `ErrorPageRenderer` — replace the built-in HTML error pages

```rust,no_run
# use autumn_web::error_pages::{ErrorContext, ErrorPageRenderer};
# use maud::{html, Markup};
# struct MyRenderer;
#
# impl ErrorPageRenderer for MyRenderer {
#     fn render_404(&self, ctx: &ErrorContext) -> Markup {
#         html! { h1 { "Custom 404 for " (ctx.path) } }
#     }
#
#     fn render_500(&self, ctx: &ErrorContext) -> Markup {
#         html! {
#             h1 { "Something went wrong." }
#             @if let Some(id) = &ctx.request_id {
#                 p { "Request ID: " (id) }
#             }
#         }
#     }
#
#     fn render_422(&self, ctx: &ErrorContext) -> Markup {
#         html! {
#             h1 { "Validation failed" }
#             p { (ctx.message) }
#         }
#     }
#
#     fn render_error(&self, ctx: &ErrorContext) -> Markup {
#         html! { h1 { (ctx.status.as_u16()) " " (ctx.message) } }
#     }
# }
# use autumn_web::prelude::*;
#[autumn_web::main]
async fn main() {
    autumn_web::app()
        .error_pages(MyRenderer)
        .routes(routes![/* ... */])
        .run()
        .await;
}
```

`ErrorPageRenderer` is the original tier-1 install in the framework — the
shape the others were modelled on. See its
[trait docs](../../autumn/src/error_pages/renderer.rs) for the full method
surface.

`ErrorContext` gives you the status code, request path, request ID, and
validation details (for `422`). Autumn only uses this renderer for requests
that prefer HTML (`Accept: text/html`). JSON handlers continue to receive the
standard structured JSON error shape.

---

## Distributing a custom subsystem as a crate

If you want to ship one of these tier-1 replacements for someone else to
install with a single line, wrap it in a `Plugin`:

```rust,no_run
use autumn_web::app::AppBuilder;
use autumn_web::plugin::Plugin;

pub struct AwsSecretsConfigPlugin {
    region: String,
}

impl AwsSecretsConfigPlugin {
    pub fn new(region: impl Into<String>) -> Self {
        Self { region: region.into() }
    }
}

impl Plugin for AwsSecretsConfigPlugin {
    fn build(self, app: AppBuilder) -> AppBuilder {
        app.with_config_loader(AwsSecretsConfigLoader::new(self.region))
    }
}
# pub struct AwsSecretsConfigLoader;
# impl AwsSecretsConfigLoader { pub fn new(_: String) -> Self { Self } }
# impl autumn_web::config::ConfigLoader for AwsSecretsConfigLoader {
#     async fn load(&self) -> Result<autumn_web::config::AutumnConfig, autumn_web::config::ConfigError> { unimplemented!() }
# }
```

End user:

```rust,no_run
# use autumn_web::prelude::*;
# struct AwsSecretsConfigPlugin;
# impl AwsSecretsConfigPlugin { fn new(_: &str) -> Self { Self } }
# impl autumn_web::plugin::Plugin for AwsSecretsConfigPlugin {
#     fn build(self, app: autumn_web::app::AppBuilder) -> autumn_web::app::AppBuilder { app }
# }
#[autumn_web::main]
async fn main() {
    autumn_web::app()
        .plugin(AwsSecretsConfigPlugin::new("us-east-1"))
        .run()
        .await;
}
```

This is the tier-3 distribution pattern. See
[`extensibility.md`](extensibility.md) for the full picture, and
[`autumn/src/plugin.rs`](../../autumn/src/plugin.rs) for naming conventions
on first-party (`autumn-<name>-plugin`) vs third-party
(`autumn-plugin-<name>`) plugin crates.

---

## What if I need to replace something that doesn't have a `with_*` method?

Three options:

1. **Most likely**: there's a tier-2 (`#[intercept]`) or built-in extension
   point that already does what you need. Check the relevant module's
   rustdoc.
2. **If it's a runtime extension**: use `AppBuilder::with_extension(value)`
   to install an arbitrary typed value. Your code retrieves it via
   `state.extension::<T>()` from request handlers or other framework code.
3. **If it's a real subsystem gap**: file an issue. Adding a new tier-1
   `with_<subsystem>` method follows a well-trodden pattern (the
   `S-053` PR added four of them at once).
