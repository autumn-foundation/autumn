# Feature Flags

Feature flags let you ship code in a disabled state and then activate it for
specific users, groups, or percentage rollouts — **without a redeploy**.  The
flag store is pluggable so you can start with an in-memory store during
development, switch to Postgres in production, and keep a consistent API
throughout.

---

## Quick start

### 1. Register the flag store

```rust
use autumn_web::feature_flags::InMemoryFlagStore;

autumn_web::app()
    .with_flag_store(InMemoryFlagStore::new())
    .routes(routes![dashboard])
    .run()
    .await;
```

In production, replace `InMemoryFlagStore` with the Postgres-backed store so
that flags survive restarts and propagate across replicas:

```rust
use autumn_web::feature_flags::{InMemoryFlagStore, FlagStore, pg::PgFlagStore};
use std::sync::Arc;

// `from_database_config` yields `None` unless the configured primary names
// Postgres — this store opens a `PgConnection` and notifies through
// `pg_notify`, so it cannot serve a `sqlite://` target.
let store: Arc<dyn FlagStore> = PgFlagStore::from_database_config(&config.database)
    .map_or_else(
        || Arc::new(InMemoryFlagStore::new()) as Arc<dyn FlagStore>,
        |store| Arc::new(store) as Arc<dyn FlagStore>,
    );

autumn_web::app()
    .with_flag_store(store)
    .run()
    .await;
```

Run the bundled migration to create the `autumn_feature_flags` and
`feature_flag_changes` tables:

```sh
autumn migrate
```

### 2. Gate a handler

The `Flags` extractor resolves the current actor from the session and exposes
a `flags.enabled(key)` method:

```rust
use autumn_web::prelude::*;
use autumn_web::feature_flags::Flags;

#[get("/dashboard")]
async fn dashboard(flags: Flags) -> Markup {
    html! {
        @if flags.enabled("beta_inbox") {
            (render_beta_inbox())
        } @else {
            (render_classic_inbox())
        }
    }
}
```

### 3. Gate a whole handler with the macro

The `#[feature_flag]` attribute macro returns 404 when the flag is disabled
and the actor is not in any allow-list:

```rust
use autumn_web::prelude::*;

#[get("/new-dashboard")]
#[feature_flag("beta_dashboard")]
async fn new_dashboard() -> &'static str {
    "New dashboard"
}
```

Supply a custom fallback handler for a nicer response:

```rust
#[get("/new-dashboard")]
#[feature_flag("beta_dashboard", fallback = upgrade_prompt)]
async fn new_dashboard() -> &'static str {
    "New dashboard"
}

async fn upgrade_prompt() -> impl IntoResponse {
    (StatusCode::FORBIDDEN, "This feature is not yet enabled for your account.")
}
```

---

## Evaluation order

For a given `(flag, actor)` pair, rules are checked in this order:

| Priority | Rule                   | Result when…                                                                       |
|----------|------------------------|------------------------------------------------------------------------------------|
| 1        | **Kill switch**        | `enabled = false` → **always off** (overrides all other rules)                     |
| 2        | **Global on**          | `enabled = true` AND `rollout_pct >= 100` → **always on** for every actor          |
| 3        | **Actor allowlist**    | `enabled = true` AND the actor's ID is in `actor_allowlist` → on for that actor    |
| 4        | **Group allowlist**    | `enabled = true` AND the actor belongs to a group in `group_allowlist` → on        |
| 5        | **Percent rollout**    | `enabled = true` AND actor's deterministic bucket < `rollout_pct` → on             |
| 6        | **Default**            | Returns `false` (fail-closed)                                                      |

> **Note:** `enabled = true` alone does **not** enable a flag for all actors — it
> only means the kill switch is off.  To enable for everyone set `rollout_pct = 100`
> (e.g. `autumn flags enable <key>` which sets both `enabled = true` and
> `rollout_pct = 100`).  Setting `enabled = false` is an instant kill switch that
> overrides rollout percentages and all allowlists.

---

## Kill switches

A flag with `enabled = false` and no allow-lists acts as a kill switch: it is
off for every actor regardless of rollout percentage.  You can disable any
feature instantly at the CLI:

```sh
autumn flags disable dark_mode
```

This writes to the database and broadcasts a `NOTIFY autumn_flags` message.
All replicas listening on that channel invalidate their cache and pick up the
new state within one cache-refresh window (default: 1 second).

---

## Percent rollouts

Percent-rollout buckets are computed with a FNV-1a hash over the UTF-8
encoding of `"<flag_name>:<actor_id>"` modulo 100.  This means:

- **Stable**: the same actor always lands in the same bucket across restarts.
- **Independent**: changing `rollout_pct` only shifts the cohort boundary, it
  does not reassign any existing actor's bucket.
- **No external dependency**: the hash is computed in-process without a
  third-party library.

```sh
autumn flags set-rollout dark_mode 25   # enable for 25% of actors
autumn flags set-rollout dark_mode 100  # roll out to everyone
```

---

## Actor allowlists

Add individual actors to the allowlist for early-access testing:

```sh
autumn flags allow dark_mode user:42
```

Allowlists are evaluated before the percent rollout, so allowlisted actors
always see the feature even if their bucket is above the rollout threshold.

---

## Group allowlists

Register a group resolver at startup to enable named-group gates:

```rust
use autumn_web::feature_flags::{FeatureFlagService, GroupResolver, InMemoryFlagStore};
use std::sync::Arc;

let store = Arc::new(InMemoryFlagStore::new());
let service = FeatureFlagService::new(store)
    .with_group_resolver(Arc::new(|actor_id, group| {
        // Return true when actor_id is a member of group.
        is_in_group(actor_id, group)
    }));
```

Then add groups to a flag:

```rust
service.add_group("beta_feature", "beta_testers", Some("cli")).unwrap();
```

---

## CLI reference

```
autumn flags list                       # list all flags with their current state
autumn flags enable <key>               # globally enable a flag (all actors)
autumn flags disable <key>              # globally disable a flag
autumn flags set-rollout <key> <pct>    # enable for pct% of actors (0–100)
autumn flags allow <key> <actor_id>     # add actor_id to the explicit allowlist
```

---

## Admin UI

Register `FeatureFlagAdminModel` with the admin plugin for a web-based flag
management panel:

```rust
use autumn_admin_plugin::{AdminPlugin, prelude::*};
use autumn_admin_plugin::feature_flags::FeatureFlagAdminModel;

autumn_web::app()
    .plugin(
        AdminPlugin::new()
            .register(FeatureFlagAdminModel::default()),
    )
    .run()
    .await;
```

The panel is mounted at `/admin/feature-flags/` and provides:

- **List view**: key, enabled status, rollout %, actor allowlist
- **Edit view**: toggle enabled, set rollout %, manage allowlists
- **History tab**: per-flag audit trail from `feature_flag_changes`

---

## Testing

Use `InMemoryFlagStore` and `TestApp::with_flag_store` to control flags in
tests without touching a database:

```rust
use autumn_web::feature_flags::InMemoryFlagStore;
use autumn_web::test::TestApp;
use std::sync::Arc;

#[tokio::test]
async fn beta_inbox_is_hidden_by_default() {
    let client = TestApp::new()
        .with_flag_store(InMemoryFlagStore::new())
        .routes(routes![dashboard])
        .build();

    client.get("/dashboard").send().await.assert_ok();
    // beta inbox is NOT present when flag is off
}

#[tokio::test]
async fn beta_inbox_renders_when_flag_enabled() {
    let store = InMemoryFlagStore::new();
    store.enable("beta_inbox", None).unwrap();

    let client = TestApp::new()
        .with_flag_store(store)
        .routes(routes![dashboard])
        .build();

    client
        .get("/dashboard")
        .send()
        .await
        .assert_ok()
        .assert_body_contains("beta-inbox");
}
```

---

## Pluggable `FlagStore`

Implement `FlagStore` to back flags with any storage system (Redis, etcd,
LaunchDarkly SDK, etc.):

```rust
use autumn_web::feature_flags::{
    FlagChangeRecord, FlagConfig, FlagStore, FlagStoreError,
};

struct MyFlagStore { /* ... */ }

impl FlagStore for MyFlagStore {
    fn get(&self, key: &str) -> Result<Option<FlagConfig>, FlagStoreError> {
        // ...
    }
    // implement the remaining methods
}

autumn_web::app()
    .with_flag_store(MyFlagStore { /* ... */ })
    .run()
    .await;
```
