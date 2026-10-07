# TD-008: Harvest Topology Progression From Embedded To External

- Status: Superseded by ADR-0016 (ownership moved to Autumn Harvest)
- Date superseded: 2026-10-07
- Date: 2026-04-09

> **Superseded.** The embedded / split / external topology described here
> shipped in `autumn-harvest-plugin` (`HarvestMode`, `worker_enabled`,
> `scheduler_enabled`). Harvest topology is now a Harvest decision, recorded in
> the [Autumn Harvest repository](https://github.com/autumn-foundation/autumn-harvest).
> See [ADR-0016](0016-durable-workflows-live-in-harvest.md) for the boundary
> between Autumn jobs and Harvest. Crate names below (`autumn-web-harvest`,
> `autumn_web_harvest`) are historical. The shipped crate is
> `autumn-harvest-plugin`. This record is kept unchanged below for history.

## Context

Today `autumn-web-harvest` embeds Harvest directly into an Autumn web app:

- Harvest startup migrates the Harvest storage role explicitly instead of
  piggybacking on the app migration list.
- Harvest startup resolves a Harvest-specific database pool.
- Activities in examples such as `reddit-clone` read and write business tables through that same pool.
- Durable app-to-Harvest publication is handled by a framework-owned outbox in
  the app database.

This is the right default for small apps. It gives the "just turn on workflows"
story with one database URL, one process, one migration path, and essentially no
operational ceremony.

It is also too coupled for the long game. Once an app grows large enough to want
Harvest isolation, the current shape forces application data access, Harvest
system storage, worker startup, and migration ownership to move together. That
turns the escape hatch into a refactor instead of a deployment decision.

Autumn should follow the same product philosophy as `bookmarks` and
`bookmarks-distributed`: start with the happy path, then make the grown-up path
an explicit, boring upgrade rather than a betrayal of the original API.

## Decision

Autumn Harvest will support one programming model across three deployment modes:

1. `embedded`
   Harvest uses the app database by default and runs worker/scheduler in the web
   process.
2. `split`
   Harvest uses a separate logical database on the same Postgres instance, while
   the worker and scheduler may still run in the web process.
3. `external`
   Harvest uses a separate Postgres cluster, while runtime ownership is decided
   by `worker_enabled` and `scheduler_enabled`. The web app can keep Harvest's
   API and outbox while a separate process owns the worker and scheduler.

The key architectural rule is:

`Harvest system storage and application business storage are distinct roles even when they point at the same DSN.`

That means:

- Harvest runtime persistence must resolve through a Harvest-specific database
  configuration and pool.
- Application handlers and activities that need business data must resolve that
  through an app-specific pool or application state seam.
- In `embedded` mode, both roles may use the same Postgres database.
- In `split` and `external` modes, they diverge by configuration instead of by
  API or macro surface.

## Configuration Direction

The intended configuration model is:

```toml
[database]
url = "postgres://app:app@localhost:5432/my_app"

[harvest]
mode = "embedded" # or "split" or "external"
worker_enabled = true
scheduler_enabled = true

[harvest.database]
url = "postgres://harvest:harvest@localhost:5432/my_app_harvest"
```

Rules:

- `embedded`: `harvest.database.url` is optional; if omitted, Harvest reuses the
  app database URL.
- `split`: `harvest.database.url` is required and should point at a separate
  logical database on the same Postgres instance.
- `external`: `harvest.database.url` is required and should point at a separate
  Postgres cluster; in-process workers are optional rather than assumed.

Example external web app config:

```toml
[database]
url = "postgres://app:app@localhost:5432/reddit_app"

[harvest]
mode = "external"
worker_enabled = false
scheduler_enabled = false

[harvest.database]
url = "postgres://harvest:harvest@harvest-cluster:5432/reddit_harvest"
```

Example dedicated Harvest runner bootstrap:

```rust
use autumn_harvest::HarvestBuilder;
use autumn_web::config::DatabaseConfig;
use autumn_web_harvest::{HarvestRunner, HarvestRunnerResources, HarvestRuntimeConfig};

let config = HarvestRuntimeConfig::load()?;
let harvest_pool = autumn_web::db::create_pool(&DatabaseConfig {
    url: config.database.url.clone(),
    ..DatabaseConfig::default()
})?
.expect("harvest runner requires a database");

let runner = HarvestRunner::start(
    HarvestBuilder::new()
        .workflows(workflows)
        .activities(activities)
        .dags(dags)
        .build(),
    &config,
    HarvestRunnerResources::new(harvest_pool),
)?;
```

## Trade-offs

### Gains

- Keeps the small-app experience frictionless.
- Makes growth a configuration and operations change instead of a programming
  model rewrite.
- Allows separate retention, backup, connection-pool sizing, and tuning for
  Harvest state.
- Preserves a clear product story: same framework surface, different topology.

### Losses

- Adds explicit Harvest configuration and a second database role to the mental
  model.
- Requires more careful state injection for activities that touch application
  tables.
- Removes any illusion that app writes and Harvest persistence remain one
  atomic transaction after leaving `embedded` mode.

## Consequences

- `autumn-web-harvest` must stop assuming `AppState.pool()` is the Harvest
  system store.
- The adapter should inject distinct state for Harvest storage and application
  storage, even if both use the same DSN in `embedded` mode.
- Migrations must target the Harvest database role explicitly once
  `harvest.database.url` is configured.
- The app database should own the durable outbox table, with framework-managed
  leasing and retry semantics for publication into Harvest storage.
- The reusable runtime-ownership seam should stay below `HarvestExt`, so the
  web app and a dedicated runner process reuse the same bootstrap path instead
  of maintaining two registration models.
- The app-to-Harvest boundary must become idempotent. For `split` and
  `external`, when application writes and workflow starts/signals must stay
  durable together, the supported pattern should be outbox plus replay-safe
  delivery rather than fake cross-database atomicity.
- Examples should tell the growth story explicitly:
  `reddit-clone` remains the embedded happy path; a future distributed sibling
  should demonstrate the larger topology.

## Non-Goals

- Exactly-once delivery across separate application and Harvest databases
- Multi-region active/active Harvest clusters
- Non-Postgres Harvest storage backends
- Immediate rewrite of all existing examples to distributed topology
