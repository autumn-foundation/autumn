# A/B Experiments

Autumn ships a first-party A/B experiment system that handles the hard parts: **deterministic bucketing, sticky assignments, and structured exposure telemetry**.  You declare experiments, start them, and assign actors — the framework emits exposure events that your analytics pipeline can join to outcome data.

---

## Quick start

### 1. Register the experiment store

```rust
use autumn_web::experiments::InMemoryExperimentStore;

autumn_web::app()
    .with_experiment_store(InMemoryExperimentStore::new())
    .routes(routes![checkout])
    .run()
    .await;
```

In production, the Postgres-backed store persists assignments across restarts. Replicas that run `PgExperimentStore::spawn_poll_listener` poll for weight changes:

```sh
autumn migrate          # creates autumn_experiments, autumn_experiment_assignments, etc.
```

### 2. Declare and start an experiment

Declare experiments at startup (or via the admin UI / CLI):

```rust
use autumn_web::experiments::{ExperimentConfig, ExperimentService, VariantConfig};
use std::sync::Arc;

let svc: ExperimentService = /* from AppState */;

svc.create(ExperimentConfig::new("checkout_v2", vec![
    VariantConfig::new("control",   50),
    VariantConfig::new("treatment", 50),
])).unwrap();

svc.start("checkout_v2").unwrap();
```

### 3. Assign an actor in a handler

The `Experiments` extractor resolves the actor from the session and the request ID from the `x-request-id` header:

```rust
use autumn_web::prelude::*;
use autumn_web::experiments::Experiments;

#[get("/checkout")]
async fn checkout(exps: Experiments) -> AutumnResult<Markup> {
    let variant = exps.assign("checkout_v2")?;
    Ok(html! {
        @match variant.as_str() {
            "treatment" => (render_new_checkout()),
            _           => (render_classic_checkout()),
        }
    })
}
```

Or use the service directly with an explicit actor:

```rust
let variant = experiments.assign("checkout_v2", "user:42")?;
```

---

## Assignment algorithm

Assignment is **deterministic per `(experiment_name, actor_id)`**:

1. Compute `bucket = FNV-1a_64("<experiment>:<actor>") mod 10 000`.
2. Map bucket to a variant proportionally by weight.

The same actor always gets the same bucket — across requests, restarts, and replicas.  Changing the hash function (never done without a migration) would re-bucket every actor in every running experiment.

### Weights

Weights are **relative**: `[("control", 30), ("treatment", 70)]` gives a 30/70 split.  They do not need to sum to 100.

```rust
VariantConfig::new("control",   30)   // 30%
VariantConfig::new("treatment", 70)   // 70%
```

Use weight `0` to disable a variant without removing it:

```rust
VariantConfig::new("dead_end", 0)     // never assigned
```

---

## Sticky assignments

Once an actor is assigned, the assignment is **recorded and returned on all subsequent calls** (`InMemoryExperimentStore` keeps it in memory; the Postgres store persists across restarts).

Changing weights **does not re-bucket** already-assigned actors — only new actors see the updated distribution.

---

## Exposure telemetry

Every `assign()` call on a `Running` experiment emits one [`ExposureRecord`]:

```json
{
  "experiment": "checkout_v2",
  "variant":    "treatment",
  "actor":      "user:42",
  "request_id": "req-abc123",
  "is_override": false,
  "timestamp_secs": 1748000000
}
```

The default sink logs at `INFO` via `tracing`.  Supply a custom [`ExposureSink`] to forward events to Segment, PostHog, or your data warehouse:

```rust
use autumn_web::experiments::{ExposureSink, ExposureRecord};
use std::sync::Arc;

struct SegmentSink { write_key: String }

impl ExposureSink for SegmentSink {
    fn record(&self, e: ExposureRecord) {
        // POST to Segment Track API
    }
}

autumn_web::app()
    .with_experiment_store_and_sink(
        InMemoryExperimentStore::new(),
        Arc::new(SegmentSink { write_key: "...".into() }),
    )
    .run()
    .await;
```

---

## Experiment lifecycle

```
draft ──► running ──► concluded
   │                  │
   └──────── archived ┘   (from any state)
```

| State       | `assign()` behaviour                                       |
|-------------|-------------------------------------------------------------|
| `Draft`     | Returns `Err(NotRunning)`                                  |
| `Running`   | Normal assignment + exposure emission                       |
| `Concluded` | Returns winner for all actors; **no** new exposures        |
| `Archived`  | Returns `Err(Archived)`                                    |

Lifecycle transitions via service:

```rust
svc.start("checkout_v2").unwrap();
svc.conclude("checkout_v2", "treatment").unwrap();
svc.archive("checkout_v2").unwrap();
```

---

## Staff / QA overrides

Pin a specific actor to a variant, bypassing the bucket calculation:

```rust
svc.set_override("checkout_v2", "qa:alice", "treatment").unwrap();
```

Overrides are tagged with `is_override = true` in exposure events so analytics pipelines can exclude them from significance calculations.

---

## Mutual exclusion groups

Prevent actors from being enrolled in multiple overlapping experiments (to avoid interaction effects):

```rust
svc.create(
    ExperimentConfig::new("exp_a", variants_a)
        .exclusion_group("checkout"),
).unwrap();
svc.create(
    ExperimentConfig::new("exp_b", variants_b)
        .exclusion_group("checkout"),
).unwrap();
```

Once an actor is assigned to `exp_a`, `assign("exp_b", actor)` returns `Err(ExcludedByGroup)`.

---

## CLI

```sh
# List all experiments
autumn experiments list

# Show details for one experiment
autumn experiments status checkout_v2

# Update weights (existing assignments unchanged)
autumn experiments set-weights checkout_v2 control=30,treatment=70

# Conclude and pin a winner
autumn experiments conclude checkout_v2 treatment

# Pin a QA actor to a specific variant
autumn experiments override checkout_v2 qa@example.com treatment
```

---

## Admin UI

Register the `ExperimentAdminModel` in the admin plugin to get a management page at `/admin/experiments/`:

```rust
use autumn_admin_plugin::{AdminPlugin, prelude::*};
use autumn_admin_plugin::experiments::ExperimentAdminModel;

autumn_web::app()
    .plugin(
        AdminPlugin::new()
            .register(ExperimentAdminModel::default()),
    )
    .run()
    .await;
```

The page includes:
- **List view**: name, state, winner
- **Edit view**: state transitions, variant weight editing
- **History tab**: per-experiment audit trail

---

## Testing

Use `InMemoryExperimentStore` and `RecordingExposureSink` in unit tests:

```rust
use autumn_web::experiments::{
    ExperimentConfig, ExperimentService, InMemoryExperimentStore,
    RecordingExposureSink, VariantConfig,
};
use std::sync::Arc;

let (sink, records) = RecordingExposureSink::new();
let store = Arc::new(InMemoryExperimentStore::new());
let svc = ExperimentService::new(store)
    .with_exposure_sink(Arc::new(sink));

svc.create(ExperimentConfig::new("checkout_v2", vec![
    VariantConfig::new("control",   50),
    VariantConfig::new("treatment", 50),
])).unwrap();
svc.start("checkout_v2").unwrap();

let variant = svc.assign("checkout_v2", "user:1").unwrap();
assert_eq!(records.lock().unwrap().len(), 1);

// Verify sticky assignment
let again = svc.assign("checkout_v2", "user:1").unwrap();
assert_eq!(variant, again);
```

---

## Out of scope

- **Analytics / significance**: autumn emits exposures; downstream tools join them to outcomes.
- **Client-side delivery**: server-rendered assignment only.
- **Multi-armed bandits**: fixed-weight experiments only.
- **Cross-experiment causal inference**: use mutual exclusion groups for isolation.
