# ADR 0005: Adopt a Plugin Trait for Autumn Integrations

- Status: Accepted
- Date: 2026-04-18
- Deciders: Autumn maintainers
- Tags: plugins, api-design, ecosystem, extensibility

## Context

Before this decision, integrations extended Autumn by defining an **extension
trait** on `AppBuilder`. The Harvest durable-workflow integration shipped a
`HarvestExt` trait with seven fluent methods (`workflows`, `activities`,
`dags`, `state`, `worker`, `harvest_api`, `harvest_api_with_auth`). Each
method had to stay idempotent across multiple calls, so `HarvestExt` paid for
that with roughly a hundred lines of bookkeeping:

- an `Any`-typed builder extension (`HarvestIntegration`) to accumulate
  state across calls,
- a shared `Arc<Mutex<HarvestIntegrationShared>>` so startup and shutdown
  closures could observe the same configuration,
- two boolean latches (`hooks_registered`, `api_route_registered`) to
  prevent the lifecycle hooks and nested router from being attached twice.

The pattern worked, but it does not scale to an ecosystem. Every integration
crate that wants to attach to an app would need to invent the same
idempotency bookkeeping, or users would register the same infrastructure
twice by accident. Authoring a reusable "drop this in your app" component is
more ceremony than it should be. There is also no obvious place to document
what the authoring contract *is* — nothing about `HarvestExt` tells a third-
party author "this is the shape you should copy."

## Decision

Autumn adopts a single, first-class `Plugin` trait in `autumn-web`. Users
compose plugins with `AppBuilder::plugin` or the tuple-taking
`AppBuilder::plugins`. Each plugin's `build(self, app)` runs exactly once per
builder; duplicate registrations (matched by `Plugin::name`) warn and are
skipped.

```rust
pub trait Plugin: Sized + Send + 'static {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed(std::any::type_name::<Self>())
    }
    fn build(self, app: AppBuilder) -> AppBuilder;
}
```

The in-tree Harvest integration moves from `HarvestExt` to `HarvestPlugin` in
a crate renamed from `autumn-web-harvest` to `autumn-harvest-plugin`. The
reddit-clone example validates the trait against a second integration with a
tiny in-tree `LiveFeedPlugin`.

## Design Details

### Consuming `self`, not `&mut App`

`Plugin::build` takes `self` by value and returns an `AppBuilder` by value.
This matches Autumn's existing builder style (`AppBuilder::routes`,
`nest`, `on_startup`, etc.) and keeps per-plugin configuration methods
zero-overhead monomorphized code. The direct consequence is that `Plugin` is
not object-safe — there is no `Box<dyn Plugin>`. Authors who want dynamic
plugin collections can hide their plugins behind an explicit enum or a
purpose-built trait object.

Bevy uses `fn build(&self, &mut App)` precisely so plugins can be object-
safe and stored in a `Vec<Box<dyn Plugin>>`. Autumn deliberately trades that
runtime flexibility for config ergonomics: calls like
`HarvestPlugin::new().workflows(...).api("...")` are plain method chains on
an owned struct.

### `Cow<'static, str>` for `Plugin::name`

`name` returns `Cow<'static, str>` rather than `&'static str`. Plugins that
only need the default `type_name` identifier pay no allocation cost
(`Cow::Borrowed`). Plugins that want to register multiple instances with
names derived from runtime configuration (sharded infrastructure, multi-
tenant connectors) can return `Cow::Owned(format!(...))` without leaking
memory.

### Warn-and-skip on duplicates

`AppBuilder::plugin` maintains a `HashSet<String>` of registered plugin
names. A duplicate registration emits a `tracing::warn!` and returns the
builder unchanged. `AppBuilder` is annotated `#[track_caller]` so the warn
points at the user's call site, not into framework internals.

This is deliberately looser than a compile-time error or a hard panic: a
user reloading a dev server, or a cooperative plugin that self-registers
inside another plugin's `build`, should not blow up the app. Plugins can
query via `AppBuilder::has_plugin(name)` to branch on whether a sibling is
already present.

### Ecosystem naming

First-party plugin crates live at `autumn-<name>-plugin` (so they cluster in
crates.io next to the crate they extend). Third-party crates use the
`autumn-plugin-<name>` prefix for discoverability. Each crate exposes a
`<Name>Plugin` struct at its root with `::new()` and `#[must_use]` fluent
configuration methods. `docs/plugins.md` holds the authoring conventions.

## Consequences

### Positive

- Third-party authors get a single, named authoring contract to copy.
- Integrations can be composed inline (`.plugins((A, B, C))`) instead of
  threaded through free functions.
- ~100 lines of idempotency bookkeeping disappear from the Harvest
  integration, and never need to be re-invented by future integrations.
- `Plugin::build` has the full `AppBuilder` surface, so a plugin can own
  routes, tasks, migrations, startup/shutdown hooks, and extension state
  as one unit — a "feature" becomes a self-contained module.

### Negative

- `Plugin` is not object-safe. Dynamic plugin collections need user-side
  enums or trait objects.
- Route conflicts between plugins surface at router-build time without
  attribution to which plugin introduced them. `has_plugin` exists but is
  advisory, not an enforcement mechanism.
- Duplicate-detection is string-based. Two different concrete plugin types
  that both override `name` to the same string will collide even though
  they are semantically different.

### Risks

- Plugin authors might reach for `name` overrides to work around a route
  conflict instead of fixing the conflict. Docs steer away from this but
  cannot prevent it.
- The `Cow<'static, str>` escape hatch invites plugins that format their
  name on every `.plugin()` call. In practice each plugin is registered
  once, so the cost is negligible, but it is not zero.

## Alternatives Considered

### 1. Keep `HarvestExt`-style extension traits as the ecosystem norm

Rejected. Every integration crate would need its own Any-slot and
idempotency latches, and there is no uniform authoring contract. The
framework has nothing to point third-party authors at.

### 2. Bevy-style `fn build(&self, &mut App)` with `Box<dyn Plugin>`

Rejected. Object safety would let apps store dynamic plugin collections,
but it would force every plugin config method into `&mut self` territory
and cost a vtable hop on every configuration call. The apps Autumn is
designed for (web services composed from a handful of integrations) do not
need dynamic plugin collections.

### 3. Hard error on duplicate plugin names

Rejected. A hot-reloading dev server or a cooperative plugin that
self-registers inside another plugin's `build` should not panic. Warn-and-
skip keeps the door open for defensive re-registration and for future
`has_plugin` gating patterns.

### 4. `fn name(&self) -> &'static str`

Rejected. A plugin that wants to register multiple instances with names
derived from runtime config would have to `Box::leak` a formatted string,
or invent its own deduplication scheme on the side. `Cow<'static, str>`
costs nothing for the default `type_name` case and removes the leak trap.

## Non-Goals

- A `PluginGroup` abstraction (Bevy's name for a plugin that expands into
  several) — defer until a real case appears.
- Per-plugin route-conflict attribution. The current router panics on
  conflict; improving that is a separate concern from the plugin trait.
- A dynamic registry of named plugins discoverable at runtime.

## Follow-On Work

> **Done (issue #1601):** *"Declare which parts of the plugin-facing surface
> are stable."* The `Plugin` trait this ADR adopted gave plugin authors an
> entry point but no compatibility contract, which capped how far anyone could
> reasonably build on it. `autumn_web::plugin_contract::PLUGIN_SURFACES` now
> declares every plugin-facing API `stable` or `experimental`, `Plugin::contract`
> lets a plugin declare the `autumn-web` range it supports (an excluded pairing
> fails at registration naming both versions), and a pinned reference plugin
> compiled in CI turns a stable-surface break into a red check. See
> [`docs/plugins.md`](../plugins.md#the-plugin-api-contract) and
> [`STABILITY.md`](../../STABILITY.md#the-plugin-api-surface-issue-1601).
>
> This does not revisit any decision recorded above: `contract` is a defaulted
> method, so the warn-and-skip duplicate behaviour, the `self`-consuming
> `build`, and the `Cow<'static, str>` name all stand unchanged.

- Re-export `Plugin` from `autumn_web::prelude` once the authoring contract
  has been exercised by more than one first-party integration.
- Consider a `#[plugin]` proc-macro that stamps out `Default`, `::new()`,
  and the boilerplate fluent methods from struct fields.
- Add a lint or test harness that catches two plugins attempting to
  register the same route path.
