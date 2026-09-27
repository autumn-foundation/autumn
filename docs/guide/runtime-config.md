# Runtime Configuration

Runtime configuration lets you change operational knobs — rate-limit ceilings,
timeouts, retry counts, support email addresses, batch sizes — **without a
redeploy**.  Operators set a value once via the CLI or admin UI; all replicas
pick it up within seconds.

This complements `autumn.toml` (which controls structural settings that require
a restart) and the credentials store (which holds encrypted secrets).  Runtime
config is for **non-secret operational tunables** that need to change at 2 am
without waiting for CI.

---

## Quick start

### 1. Declare your keys

In your application code, build a `ConfigRegistry` and declare every key you
want to be live-tunable:

```rust
use autumn_web::runtime_config::{
    ConfigKeySchema, ConfigRegistry, ConfigValidator, ConfigValue, ConfigValueType,
};

pub fn build_config_registry() -> ConfigRegistry {
    let mut registry = ConfigRegistry::new();

    registry.define(
        ConfigKeySchema::new("max_upload_mb", ConfigValueType::Int, ConfigValue::Int(50))
            .description("Maximum upload size accepted by the API in megabytes")
            .validator(ConfigValidator::IntRange { min: Some(1), max: Some(500) }),
    ).unwrap();

    registry.define(
        ConfigKeySchema::new(
            "support_email",
            ConfigValueType::Text,
            ConfigValue::Text("support@example.com".to_owned()),
        )
        .description("Reply-to address on outbound support emails")
        .validator(ConfigValidator::Regex("[a-z0-9._%+]+@[a-z0-9.-]+".to_owned())),
    ).unwrap();

    registry.define(
        ConfigKeySchema::new("rate_limit_rps", ConfigValueType::Float, ConfigValue::Float(100.0))
            .description("Global rate limit in requests per second")
            .validator(ConfigValidator::FloatRange { min: Some(0.1), max: Some(10_000.0) }),
    ).unwrap();

    registry
}
```

### 2. Wire up the service

```rust
use autumn_web::runtime_config::{InMemoryConfigStore, RuntimeConfigService};
use std::sync::Arc;

let registry = Arc::new(build_config_registry());
let store = Arc::new(InMemoryConfigStore::new()); // swap for Postgres in production

let config_svc = RuntimeConfigService::new(registry, store);
```

### 3. Read values in handlers

```rust
let mb = config_svc.get("max_upload_mb")
    .unwrap()
    .as_int()
    .unwrap_or(50);

if upload_size_mb > mb {
    return Err(AutumnError::bad_request("Upload exceeds configured limit"));
}
```

---

## CLI

`autumn config` commands let operators inspect and mutate live config without
writing code.  All commands connect to the configured Postgres database.

```bash
# List every active override
autumn config list

# Show the current value of a single key
autumn config get max_upload_mb

# Set a new value (the running app reads it within one cache-refresh cycle)
autumn config set max_upload_mb 200 --actor ops@example.com

# Revert to the compile-time default
autumn config unset max_upload_mb

# View change history for a key
autumn config history max_upload_mb
autumn config history max_upload_mb --limit 50
```

The database URL is resolved from `autumn.toml` or the environment (same
precedence as `autumn migrate`):
1. `AUTUMN_DATABASE__PRIMARY_URL`
2. `AUTUMN_DATABASE__URL`
3. `DATABASE_URL`
4. `database.primary_url` from `autumn.toml`
5. `database.url` from `autumn.toml`

---

## Schema

### Supported types

| `ConfigValueType` | Rust type | Example raw value |
|---|---|---|
| `Int` | `i64` | `200` |
| `Float` | `f64` | `3.14` |
| `Text` | `String` | `ops@example.com` |
| `Bool` | `bool` | `true` / `false` / `yes` / `no` / `1` / `0` / `on` / `off` |
| `DurationSecs` | `u64` (seconds) | `3600` |
| `Json` | `serde_json::Value` | `{"retry":3}` |

### Validators

Attach one or more validators to a key using `.validator(...)`.  All validators
are applied in order; the first rejection wins.  A write that fails validation
is not persisted — the previous value is unchanged.

```rust
// Integer range
ConfigValidator::IntRange { min: Some(1), max: Some(100) }

// Float range
ConfigValidator::FloatRange { min: Some(0.0), max: Some(1.0) }

// Whitelist of allowed string values
ConfigValidator::AllowedValues(vec![
    "draft".to_owned(),
    "published".to_owned(),
    "archived".to_owned(),
])

// Regex (full-string match, anchored automatically)
ConfigValidator::Regex("[a-z0-9]+@[a-z0-9.]+".to_owned())
```

---

## Storage backends

Implement the `ConfigStore` trait to plug in any backend:

```rust
pub trait ConfigStore: Send + Sync + 'static {
    fn get_raw(&self, key: &str) -> Result<Option<String>, ConfigStoreError>;
    fn set_raw(
        &self,
        key: &str,
        old_raw: Option<String>,
        new_raw: String,
        actor: Option<&str>,
    ) -> Result<(), ConfigStoreError>;
    fn unset_raw(
        &self,
        key: &str,
        old_raw: Option<String>,
        actor: Option<&str>,
    ) -> Result<(), ConfigStoreError>;
    fn list_overrides(&self) -> Result<Vec<(String, String)>, ConfigStoreError>;
    fn history(
        &self,
        key: &str,
        limit: usize,
    ) -> Result<Vec<ConfigChangeRecord>, ConfigStoreError>;
}
```

| Backend | When to use |
|---|---|
| `InMemoryConfigStore` | Unit tests, local dev, single-process demos |
| Postgres (via migration) | Production default — survives restarts, shared across replicas |
| Custom | Redis, etcd, external provider — implement `ConfigStore` |

### Postgres migration

Run `autumn migrate` after adding the dependency to apply the built-in
migration that creates the `autumn_runtime_config_values` and
`autumn_runtime_config_changes` tables.

---

## Audit trail

Every write records:
- The **key** changed
- The **old value** (or `NULL` if the key was unset)
- The **new value** (or `NULL` for `unset`)
- The **actor** (passed by the caller: username, email, or `"cli"`)
- A UTC **timestamp**

Query history directly:

```sql
SELECT key, old_value, new_value, actor, changed_at
FROM autumn_runtime_config_changes
WHERE key = 'max_upload_mb'
ORDER BY changed_at DESC
LIMIT 20;
```

Or use the CLI:

```bash
autumn config history max_upload_mb
```

---

## What runtime config is NOT for

- **Secrets** (API keys, passwords): use the [credentials store](credentials.md).
- **Structural config** (bind address, DB pool size, plugin list): these belong
  in `autumn.toml` and require a restart.
- **Feature flags** (boolean on/off): see the feature-flags guide.
- **Per-tenant overrides**: blocked on the multi-tenancy milestone; revisit later.
