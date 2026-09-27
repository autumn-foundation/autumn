# Extensibility in Autumn

Autumn ships with sensible defaults for everything — config loading, the
database pool, the session store, the telemetry subscriber, error pages, the
request cache, route handlers, middleware. None of those defaults are written
in stone. The framework gives you three different mechanisms to swap them out,
each suited to a different scope of change. Knowing which tier a piece of
behaviour lives in is the fastest way to find the right hook.

This guide names the three tiers, shows which subsystems live where, and
points you at the per-tier how-tos.

---

## The three tiers at a glance

| Tier | Mechanism | Scope | Typical use case |
|------|-----------|-------|------------------|
| **1** | `AppBuilder::with_<subsystem>(impl Trait)` | Boot-time, one-per-app | Replace a framework subsystem (config loader, DB pool, session store, telemetry, error pages) |
| **2** | `#[intercept(Layer)]` | Per-request, stackable | Add cross-cutting middleware (caching, custom auth, request shaping, tracing) |
| **3** | `Plugin::build(app)` | Distribution wrapper around tier 1 + 2 | Ship a reusable integration as a crate (e.g. `autumn-aws-secrets-plugin`) |

These compose: a tier-3 `Plugin` typically does its work by calling tier-1 or
tier-2 hooks inside `build()`. There is no fourth tier — if you find yourself
reaching for one, file an issue.

---

## Tier 1: boot-time subsystem replacement

Tier-1 hooks let you replace a framework subsystem with your own
trait-implementing struct. Each is a fluent `with_<subsystem>` method on
`AppBuilder`. They run exactly once during `AppBuilder::run()`, before the
HTTP server starts.

```rust,no_run
use autumn_web::prelude::*;

#[autumn_web::main]
async fn main() {
    autumn_web::app()
        .with_config_loader(MyJsonConfigLoader::new("config.json"))
        .with_telemetry_provider(DatadogTelemetryProvider)
        .with_session_store(MyEncryptedCookieStore)
        .routes(routes![/* ... */])
        .run()
        .await;
}
```

### Subsystems available at tier 1

| Subsystem | Trait | Builder method | Default |
|-----------|-------|----------------|---------|
| Config loading | [`ConfigLoader`](../../autumn/src/config.rs) | `with_config_loader` | `TomlEnvConfigLoader` (five-layer TOML + env) |
| Database pool | [`DatabasePoolProvider`](../../autumn/src/db.rs) | `with_pool_provider` | `DieselDeadpoolPoolProvider` (deadpool + diesel-async) |
| Telemetry | [`TelemetryProvider`](../../autumn/src/telemetry.rs) | `with_telemetry_provider` | `TracingOtlpTelemetryProvider` (`tracing-subscriber` + optional OTLP) |
| Session store | [`SessionStore`](../../autumn/src/session.rs) | `with_session_store` | `MemoryStore` or `RedisStore` based on `session.backend` config |
| Error pages | [`ErrorPageRenderer`](../../autumn/src/error_pages/renderer.rs) | `error_pages` | Built-in HTML renderer |

See **[`custom-subsystems.md`](custom-subsystems.md)** for a per-subsystem
how-to with full code examples.

### When tier 1 is the right answer

- You want **one** subsystem behaving differently for the **whole app**.
- The replacement happens at **boot time**, not per-request.
- You want **type-checked** integration with the rest of the framework.

---

## Tier 2: per-request middleware via `#[intercept]`

Tier-2 hooks attach a tower-style `Layer` to a route or group of routes. They
run once per matching request and can stack arbitrarily.

```rust,no_run
# use autumn_web::prelude::*;
# #[derive(Clone)]
# struct CacheExpensive;
# impl<S> tower::Layer<S> for CacheExpensive {
#     type Service = S;
#     fn layer(&self, inner: S) -> S { inner }
# }

#[get("/expensive")]
#[intercept(CacheExpensive)]
async fn expensive() -> &'static str {
    "computed once, served many"
}
```

> The attribute argument is parsed as a path and used as a value, so the layer
> must be nameable as one — a unit struct, or a `const`/`static`. `Layer::new(..)`
> does not parse and is currently dropped **silently**. See the
> [middleware guide](./middleware.md) for the full constraint.

### When tier 2 is the right answer

- You're augmenting **request handling**, not boot-time behaviour.
- You want the change to apply to **a subset** of routes (or all of them, but
  via stacking rather than replacement).
- Multiple instances should be **layered**, not "last one wins".

### Tier-2 examples

- **Caching** — a response-cache layer is the
  intentional path for response caching. The `Cache` trait isn't a tier-1
  install because there is no single "framework cache" — different routes
  benefit from different cache configurations.
- **Custom tracing** — wrap a route with `#[intercept(TracingLayer)]` to
  emit additional spans without touching the global subscriber.
- **Auth shape variants** — apply `#[intercept(BearerAuthLayer)]` to API
  routes while leaving session-cookie routes untouched.

---

## Tier 3: distribution as a `Plugin`

Tier-3 packages tier-1 and tier-2 calls into a reusable struct that anyone can
install with a single line. This is the right shape for cross-organisation
distribution — publish a crate with a `Plugin` in it, users `cargo add` it and
write `.plugin(YourPlugin::new(...))`.

```rust,no_run
use autumn_web::app::AppBuilder;
use autumn_web::plugin::Plugin;

pub struct AwsSecretsConfigPlugin {
    region: String,
}

impl Plugin for AwsSecretsConfigPlugin {
    fn build(self, app: AppBuilder) -> AppBuilder {
        // Tier-1 install inside the plugin's build() — the user just sees
        // `.plugin(AwsSecretsConfigPlugin::new("us-east-1"))`.
        app.with_config_loader(AwsSecretsConfigLoader::new(self.region))
    }
}

# pub struct AwsSecretsConfigLoader;
# impl AwsSecretsConfigLoader { fn new(_: String) -> Self { Self } }
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

### When tier 3 is the right answer

- You're packaging behaviour for **someone else** to consume.
- The behaviour spans **multiple tier-1 or tier-2 installs** and should
  travel together as a unit (e.g. an OTel plugin that installs both a
  `TelemetryProvider` AND a per-request tracing layer).
- You want **conflict detection** — two plugins claiming the same `name()`
  trigger a warning, so duplicate `.plugin(...)` calls don't silently shadow
  each other.

See [`autumn/src/plugin.rs`](../../autumn/src/plugin.rs) for the trait
definition and naming conventions for first-party vs third-party plugin
crates.

### The stability contract you get, and the one you owe

Once you are distributing to someone else, the question stops being "can I
reach this API" and becomes "will it still be here next release". Autumn
answers that explicitly (issue #1601):

- **What Autumn owes you.** Every plugin-facing API — including the four tier-1
  seams above — is declared `stable` or `experimental` in
  [the plugin API contract](../plugins.md#the-plugin-api-contract). A stable
  one follows [`STABILITY.md`](../../STABILITY.md)'s SemVer promise, is
  compiled on every commit by a pinned reference plugin, and cannot break
  without a **Plugin authors** section in that release's migration guide.
- **What you owe your users.** Implement `Plugin::contract` to declare the
  `autumn-web` range you support. An app on a framework outside it then fails
  at registration with a message naming both versions, instead of misbehaving
  in a way nobody can trace back to a version mismatch.

```rust,ignore
fn contract(&self) -> Option<autumn_web::plugin_contract::PluginContract> {
    Some(
        autumn_web::plugin_contract::PluginContract::new(env!("CARGO_PKG_NAME"))
            .plugin_version(env!("CARGO_PKG_VERSION"))
            .autumn_web("0.7"),
    )
}
```

`autumn generate plugin` scaffolds this, and
`autumn plugin-check --plugin-name <your-plugin>` verifies it.

---

## Choosing between tiers — a quick decision tree

1. **"I want to change framework behaviour for the whole app, once at boot."**
   → Tier 1.
2. **"I want to apply a wrapper to specific requests."**
   → Tier 2.
3. **"I'm building this for someone else to install with one line."**
   → Tier 3, wrapping tier-1 or tier-2 calls.

If a subsystem you want to replace doesn't have a tier-1 method yet, file an
issue — adding one is mechanical and we generally welcome the patch.

---

## Further reading

- [`custom-subsystems.md`](custom-subsystems.md) — per-trait how-to for tier-1
  hooks, with full runnable code.
- [`ConfigLoader`](https://docs.rs/autumn-web/latest/autumn_web/config/trait.ConfigLoader.html)
  — the trait behind `with_config_loader`, for sourcing config from Vault,
  Secrets Manager, or a JSON/YAML file instead of five-layer TOML.
- [`autumn/src/plugin.rs`](../../autumn/src/plugin.rs) — `Plugin` trait
  documentation, including the naming conventions for distributed plugins.
- [`plugins.md`](../plugins.md#the-plugin-api-contract) — which plugin-facing
  APIs are stable versus experimental, the SemVer policy for each tier, and how
  a plugin declares the `autumn-web` range it supports.

---

## Forwarded headers in plugins

Plugin middleware that reads `X-Forwarded-*` headers directly is one PR away
from being CVE-shaped. Autumn centralises all forwarding-header trust logic in
a single `[security.trusted_proxies]` policy that every built-in middleware
honours.

> **Rule: never read `X-Forwarded-*` directly. Use `ClientAddr`, `ClientHost`,
> or `ClientScheme` from `autumn_web::extract`.**

```rust,no_run
use autumn_web::extract::ClientAddr;
use autumn_web::prelude::*;

// Good: uses resolver-validated IP
#[get("/rate-check")]
async fn rate_check(ClientAddr(ip): ClientAddr) -> String {
    format!("your ip: {ip}")
}

// Bad: trusts attacker-controlled header
// let ip = req.headers().get("x-forwarded-for").unwrap(); // ← DO NOT DO THIS
```

See [middleware.md](./middleware.md#forwarded-header-client-identity-plugin-author-guidance)
for the full guide.
