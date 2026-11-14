# Migrating to the next Autumn release (rolling draft)

> **Rolling draft.** This is the in-flight guide for the changes that are not
> released yet. Every PR that lands a breaking change appends a section here
> and links this file from its changelog note — which is a file under
> [`changelog.d/`](../../changelog.d/README.md), not a line in
> [`CHANGELOG.md`](../../CHANGELOG.md). At release time the file is renamed to
> `docs/migrations/<version>.md`, its version placeholders are filled in, and
> the index in [`README.md`](README.md) is updated — see
> [`docs/release-checklist.md`](../release-checklist.md), *Migration Guide
> Gate*.
>
> The `{X.Y.Z}` placeholders below are deliberate: the gate treats `next.md` as
> a draft and accepts them (and empty sections) here, so nothing has to be
> invented for a release that has no changes yet.

## At a glance

- **Old version:** `autumn-web {X.Y.Z}`
- **New version:** `autumn-web {X.Z.0}`
- **Expected upgrade effort:** {S / M / L — one paragraph of context}
- **MSRV delta:** `{old MSRV}` → `{new MSRV}` ({reason, or "unchanged"})
- **Carried dependency majors:** {e.g. `axum 0.8 → 0.9`, `diesel 2 → 3`,
  or "none"}

## Summary

One paragraph describing *why* this release is major. Prefer "we want
these properties, and they required breaking change `X`" over a list of
unrelated removals.

Link to the [CHANGELOG entry](../../CHANGELOG.md) for the release for the
full commit-level picture.

## Before you start

- Pin your existing version (`autumn-web = "={X.Y.Z}"`) and commit.
- Run `cargo update` *before* the upgrade so the subsequent diff is just
  the major bump.
- Make sure your test suite is green on the old version. You will want
  the safety net.

## Step-by-step

1. **Run `autumn upgrade`** — *before* the dependency bump. The release it
   migrates from is the one your `Cargo.toml` still records, so bumping first
   leaves nothing in range. It previews every mechanical change this release
   can apply to your own source — a per-file diff plus a count of affected
   sites — and writes nothing; re-run with `--apply` to take them. Anything it
   cannot safely rewrite is listed with `file:line` and a link to the guide
   section that explains it.

   ```bash
   cargo install autumn-cli --version {X.Z.0}
   autumn upgrade            # preview
   autumn upgrade --apply    # take it
   ```

2. **Bump the dependency.**
   ```toml
   # Cargo.toml
   [dependencies]
   autumn-web = "{(X+1).0}"
   ```

3. **Run `cargo check`.** Work through the compiler errors section by
   section using the cheat sheet below. Only the changes labelled `review` or
   `manual` above should still need you.

4. **Apply configuration changes** (see
   [Configuration changes](#configuration-changes)).

5. **Run the test suite.**

6. **Run the application locally** and exercise each feature at least
   once. Pay attention to the [Behavior changes](#behavior-changes)
   section.

## Breaking changes

Repeat the block below for each breaking change. Keep changes grouped by
area (routing / config / database / …) so readers can skip to what they
care about.

### {Area}: {Short description}

**Why:** One or two sentences on the motivation.

**Before (`{X.Y}`):**

```rust
// paste a minimal, compiling example from the old version
```

**After (`{(X+1).0}`):**

```rust
// paste the equivalent on the new version
```

**Automation:** `manual` — {why no codemod applies: it needs new arguments, it
is a configuration or behaviour change, it is only reachable inside a macro, ….
For a change `autumn upgrade` *does* rewrite, use `auto` (safe by construction:
renames and import moves) or `review` (rewritten, every site flagged for a
human) instead, and name the shipped codemod id from
`autumn-cli/src/upgrade/migrations.rs` in this paragraph.}

Every breaking change carries this label — `scripts/check-migration-guides.sh`
fails without it, and fails an `auto`/`review` label that names no shipped
codemod, or a rename-level change left `manual` with no reason (issue #1629).

### Database: `DatabaseConfig` has a `roles` field

**Why:** each boundary can connect as its own Postgres role (issue #3550).
`[database.roles.<boundary>]` sets the connection of that role.

**You are affected only if your code builds a `DatabaseConfig` with a struct
literal and no `..Default::default()`.** TOML and `AUTUMN_*` config need no
change: an empty `roles` keeps one role.

**Before (`{X.Y}`):**

```rust
DatabaseConfig {
    url: Some(url),
    // ...every other field...
    fleet: None,
}
```

**After (`{(X+1).0}`):**

```rust
DatabaseConfig {
    url: Some(url),
    // ...every other field...
    fleet: None,
    roles: std::collections::BTreeMap::new(),
}
```

**Automation:** `manual` — add `roles: BTreeMap::new()`, or end the literal
with `..Default::default()`. A codemod cannot know which ending the app wants.

---

### Graph: `NodeKind`, `GraphNode` and `Completeness` have new members

**Why:** role-aware background tasks (issue #3545) appear in the
architecture graph. `NodeKind` has a new variant `BackgroundTask`. `GraphNode`
has a new field `background`. `Completeness` has a new field
`background_tasks`. `NodeKind` and `GraphNode` are not `#[non_exhaustive]`.

**You are affected only if your code matches on `NodeKind` with no wildcard
arm, or builds a `GraphNode { .. }` or `Completeness { .. }` literal.**

**Before (`{X.Y}`):**

```rust,ignore
match node.kind {
    NodeKind::Job | NodeKind::ScheduledTask | NodeKind::OneOffTask => "job",
    // …
}
let node = GraphNode { /* … */ job: None };
```

**After (`{(X+1).0}`):**

```rust,ignore
match node.kind {
    NodeKind::Job | NodeKind::ScheduledTask | NodeKind::OneOffTask => "job",
    NodeKind::BackgroundTask => "background task",
    // …
}
let node = GraphNode { /* … */ job: None, background: None };
```

`Completeness` derives `Default`. Use `..Default::default()` in a literal.

**Automation:** `manual` — a new arm needs a label that only the caller can
choose.

---

### Fleet: `DatabaseFleetConfig` and `FleetStats` have new fields

**Why:** a fleet has a memory budget next to `max_open` (issue #3642,
ADR 0040). `DatabaseFleetConfig` has a new field `max_resident_bytes`.
`FleetStats` has the new fields `resident_bytes` and `max_resident_bytes`,
and is now `#[non_exhaustive]`.

**You are affected only if your code builds a `DatabaseFleetConfig` with a
struct literal and no `..` spread, or builds a `FleetStats`.** TOML and
`AUTUMN_*` config need no change: the default `0` means no budget.

**Before (`{X.Y}`):**

```rust,ignore
DatabaseFleetConfig {
    mode: FleetMode::Tenant,
    root: "/var/lib/app/fleet".to_owned(),
    max_open: 256,
    // ...every other field...
}
```

**After (`{(X+1).0}`):**

```rust,ignore
DatabaseFleetConfig {
    mode: FleetMode::Tenant,
    root: "/var/lib/app/fleet".to_owned(),
    max_open: 256,
    max_resident_bytes: 0,
    // ...every other field...
}
```

**Automation:** `manual` — add `max_resident_bytes: 0`. A codemod cannot
know the budget the app wants. Only the fleet builds a `FleetStats`: read it
from `DatabaseFleet::stats`.

---

### Fleet: `max_open_policy` adds a field and an error

**Why:** an app can make `max_open` a hard limit (issue #3538).
`DatabaseFleetConfig` has a new field `max_open_policy`. `FleetError` has a
new variant `AtCapacity` (`503`). `FleetStats` has the new fields `peak_open`,
`over_cap_opens_total` and `refused_total`.

**You are affected only if your code builds a `DatabaseFleetConfig` with a
struct literal and no `..` spread, or matches `FleetError` without a
wildcard arm.** TOML and `AUTUMN_*` config need no change: the default
`"soft"` keeps the old behavior.

**Before (`{X.Y}`):**

```rust,ignore
DatabaseFleetConfig {
    mode: FleetMode::Tenant,
    root: "/var/lib/app/fleet".to_owned(),
    max_open: 256,
    // ...every other field...
}
```

**After (`{(X+1).0}`):**

```rust,ignore
DatabaseFleetConfig {
    mode: FleetMode::Tenant,
    root: "/var/lib/app/fleet".to_owned(),
    max_open: 256,
    max_open_policy: FleetMaxOpenPolicy::Soft,
    // ...every other field...
}
```

**Automation:** `manual` — add `max_open_policy: FleetMaxOpenPolicy::Soft`,
and a `FleetError::AtCapacity { .. }` arm (it answers `503`).

---

### Fleet: `ShipMode::Continuous` carries a shipper

**Why:** core ships the databases of every engine and checks the fence
before each write
(ADR 0040 §5). `ShipMode` has a new mode, `Generations { every }`.
`Continuous` carries the shipper that core drives, an `Arc<dyn
ContinuousShipper>`. So `ShipMode` is no longer `Copy`, `PartialEq` or `Eq`.
`ContinuousShipper` is sealed: only `SqliteEngine` uses `Continuous`.

**You are affected only if your code matches or compares a `ShipMode`, or
your engine returned `ShipMode::Continuous`.** An engine plugin that returned
`ShipMode::Rebuild` needs no change. An engine that returned
`ShipMode::Continuous` must return `ShipMode::Generations { every }`. Core
does not let a plugin give a continuous shipper.

**Before (`{X.Y}`):**

```rust,ignore
if engine.ship_mode() == ShipMode::Continuous {
    // ...
}
```

**After (`{(X+1).0}`):**

```rust,ignore
if matches!(engine.ship_mode(), ShipMode::Continuous(_)) {
    // ...
}
```

`DatabaseFleet::install_replication` now works for any engine, and its
signature does not change. `FleetStats` has the new fields `ship_mode` and
`rpo_secs`; it is `#[non_exhaustive]`, so a read needs no change.

**Automation:** `manual` — a codemod cannot know which comparison the code
wants.

---

### Database: `DatabaseConfig` and `DatabaseFleetConfig` have `mmap_size` and `cache_size_kib`

**Why:** an app can turn on SQLite memory-mapped reads and a larger page
cache. Both fields are `0` (off) by default. See
[Memory-mapped reads](../guide/sqlite-in-production.md#memory-mapped-reads-and-the-page-cache).

**You are affected only if your code builds a `DatabaseConfig` with a struct
literal and no `..Default::default()`, or builds a `DatabaseFleetConfig` with
a struct literal.** TOML and `AUTUMN_*` config need no change.

**Before (`{X.Y}`):**

```rust,ignore
DatabaseFleetConfig {
    mode: FleetMode::Tenant,
    root: "/var/lib/app/fleet".to_owned(),
    // ...every other field...
    replication_workers: 4,
}
```

**After (`{(X+1).0}`):**

```rust,ignore
DatabaseFleetConfig {
    mode: FleetMode::Tenant,
    root: "/var/lib/app/fleet".to_owned(),
    // ...every other field...
    replication_workers: 4,
    mmap_size: 0,
    cache_size_kib: 0,
}
```

Add the same two fields to a `DatabaseConfig` literal, or end it with
`..Default::default()`.

**Automation:** `manual` — `0` keeps the old behavior, but a codemod cannot
know if the app wants a different value.

### Deploy: `DeployConfig` has a `groups` field

**Why:** `[[deploy.groups]]` puts a role on a group of fleet hosts. The
field is empty by default. See
[Per-group roles](../guide/fleet-deploys.md#per-group-roles).

**You are affected only if your code builds a `DeployConfig` with a struct
literal and no `..Default::default()`.** TOML and `AUTUMN_*` config need no
change.

**Before (`{X.Y}`):**

```rust,ignore
DeployConfig {
    host: Some("10.0.0.1".to_owned()),
    // ...every other field...
    bake: DeployBakeConfig::default(),
}
```

**After (`{(X+1).0}`):**

```rust,ignore
DeployConfig {
    host: Some("10.0.0.1".to_owned()),
    // ...every other field...
    bake: DeployBakeConfig::default(),
    groups: Vec::new(),
}
```

**Automation:** `manual` — an empty list keeps the old behavior.

### Release: regenerated Kubernetes output has one workload per role

**Why:** `autumn release init --target kubernetes` now writes one workload per
role. See [One workload per role](../guide/kubernetes.md#one-workload-per-role).

**You are affected only if you run `autumn release init --target kubernetes
--force` again over an earlier output.** Files you do not regenerate keep
working.

- Object names change from `<name>` to `<name>-<role>`, for example
  `shop-combined`. An upgrade replaces the Deployment and the Service. Point
  an Ingress or a `flagger.ingressRef` outside the chart at the new Service.
- The Helm value `replicaCount` moved to `roles.<name>.replicaCount`. The
  chart refuses to render when a values file still sets `replicaCount`. Move
  the value:

  ```yaml
  # Before
  replicaCount: 5
  # After
  roles:
    combined:
      replicaCount: 5
  ```

- The Kustomize `deployment.yaml`, `service.yaml` and `pdb.yaml` moved from
  `base/` to `components/<role>/`. Change the target names in your overlay
  patches, for example `shop` to `shop-combined`.

**Automation:** `manual` — the generator cannot see your overlays and values
files.

---

### Events: `#[event]` derives `WireShape` and refuses what it cannot read

**Why:** an event payload is a wire contract between a producer and its
consumers. `#[event]` now records the payload's shape, so a `#[listener]`
that reads a field the event does not send fails the build.

**You are affected only if an `#[event]` type is generic, an enum, a tuple
struct, uses `#[serde(flatten)]`, `#[serde(transparent)]`, `#[serde(into)]`,
`#[serde(from)]`, `#[serde(try_from)]`, `#[serde(tag)]`, a split `rename`, or
also has `#[derive(WireShape)]`.** Other events need no change. Their topic
does not change.

**You are also affected if a `#[listener]` reads a field with
`#[serde(skip_serializing)]` or `#[serde(skip_deserializing)]`.** That read
never got the value. The build now fails at the read. Stop reading the
field, or remove the skip.

**Before (`{X.Y}`):**

```rust
#[event]
struct PageLoaded {
    #[serde(flatten)]
    counts: BTreeMap<String, i64>,
}
```

**After (`{(X+1).0}`):**

```rust
#[event(opaque)]
struct PageLoaded {
    #[serde(flatten)]
    counts: BTreeMap<String, i64>,
}
```

An `#[event]` that also derives `WireShape` drops the derive: `#[event]`
implements it now.

**Automation:** `manual` — `opaque` turns the check off. A codemod cannot know
if the app wants that, or a payload the check can read.

---

### Wire contracts: the export refuses a field type it cannot compare

**Why:** `autumn contracts check` compared a nested type by its name only, so
a change inside it was not seen. The export now puts each nested
`WireShape` type in the bundle. A field type with no descriptor cannot be
compared, so the export stops rather than approve it by name.

**You are affected only if you run `autumn contracts export` or
`autumn contracts check`, and a `WireShape` or `#[event]` field has a type
that is not a `WireShape` type and not a known leaf.** Known leaves are
primitives, `String`, standard containers, `serde_json::Value`, `Uuid`,
`chrono` and `time` types, `Decimal`, `Url`, and similar. An enum field is
the usual case. The build does not change.

**Before (`{X.Y}`):**

```rust
#[derive(serde::Serialize, serde::Deserialize, WireShape)]
pub struct Item {
    pub id: String,
    pub status: Status,
}
```

**After (`{(X+1).0}`):**

```rust
#[derive(serde::Serialize, serde::Deserialize, WireShape)]
pub struct Item {
    pub id: String,
    #[wire(opaque)]
    pub status: Status,
}
```

`#[wire(opaque)]` compares the field by its type name, as before. For a
struct, derive `WireShape` on it instead, so the check compares its fields.

The export also writes bundle format 3. An older CLI refuses it. Upgrade the
CLI on every job that reads the bundle. This CLI still reads formats 1 and 2.

A bare type name that two reachable crates define now stops the export.
Write the full path in the handler signature, for example
`Json<shared::Item>` or `Json<crate::dto::Item>`.

Type descriptor files now hold the module in their name. Delete
`target/autumn-contracts` once after the upgrade, so an old file does not
change a build message.

**Automation:** `manual` — only the app knows if a type should get a shape
or stay opaque.

---

### Wire contracts: bundle format 4

**Why:** each field now records its wire type, the JSON form serde writes.
The check compares it, so a new JSON form with the same Rust type is
reported. An older CLI would not compare wire types, so it refuses the new
format.

**You are affected only if a job reads a bundle from
`autumn contracts export` with an older CLI.** The build does not change.

**Before (`{X.Y}`):**

```bash
autumn contracts check --against deployed.json  # CLI {X.Y}, bundle format 3
```

**After (`{(X+1).0}`):**

```bash
autumn contracts check --against deployed.json  # CLI {(X+1).0}, bundle format 4
```

Upgrade the CLI on every job that reads the bundle. This CLI reads formats
1 to 4. When one of the two bundles is format 3 or older, the check
does not compare wire types.

**Automation:** `manual` — upgrade the CLI in each job.

---

### State: `AppState` getters are no longer `const fn`

**Why:** `AppState` holds its fields behind one `Arc`, so a clone is one
reference count (issue #3168). A `const fn` cannot read through an `Arc`.
These getters lost `const`: `pool`, `replica_pool`, `shards`, `metrics`,
`log_levels`, `task_registry`, `job_registry`, `config_props`,
`metrics_source_registry`, `health_indicator_registry`, `probes`,
`policy_registry`, `forbidden_response`, `with_forbidden_response`, `role`,
`capabilities`, `channels`, `presence` and `collab`.

**You are affected only if your code calls one of them from a `const fn`.**
No `AppState` value can exist in a const context, so such a call cannot run.

**Before (`{X.Y}`):**

```rust,ignore
const fn metrics_of(state: &AppState) -> &MetricsCollector {
    state.metrics()
}
```

**After (`{(X+1).0}`):**

```rust,ignore
fn metrics_of(state: &AppState) -> &MetricsCollector {
    state.metrics()
}
```

**Automation:** `manual` — remove `const` from the caller.

---

### Outbox: `OutboxConfig` has `destinations` and `inbox` fields

**Why:** the relay can send a topic to another service, and an app can take
messages from another service (issue #3553). `[outbox.destinations.<name>]`
and `[outbox.inbox]` set this.

**You are affected only if your code builds an `OutboxConfig` with a struct
literal and no `..` spread.** TOML and `AUTUMN_*` config need no change: no
destination and a disabled inbox keep the old behavior.

**Before (`{X.Y}`):**

```rust,ignore
OutboxConfig {
    enabled: true,
    // ...every other field...
    retention_ms: 604_800_000,
}
```

**After (`{(X+1).0}`):**

```rust,ignore
OutboxConfig {
    enabled: true,
    // ...every other field...
    retention_ms: 604_800_000,
    destinations: std::collections::BTreeMap::new(),
    inbox: OutboxInboxConfig::default(),
}
```

`TestApp::with_outbox` is no longer `const fn`. No `TestApp` value can exist
in a const context, so no call changes.

**Automation:** `manual` — add the two fields, or end the literal with
`..OutboxConfig::default()`. A codemod cannot know which ending the app
wants.

### Metrics: `MetricsSnapshot` has a `db_connection_holds` field and is non-exhaustive

**Why:** the `Db` extractor recorded its connection hold time as a query,
under `db_queries["<route> SELECT"]` (issue #3579). The hold now has its own
map, `db_connection_holds`, keyed by the route. See
[Metrics: the `"<route> SELECT"` key is deprecated](#metrics-the-route-select-key-is-deprecated).

`MetricsSnapshot` is now `#[non_exhaustive]`, so a later field does not
break your code.

**You are affected only if your code builds a `MetricsSnapshot` with a
struct literal, or destructures one without `..`.** Code that reads
`/actuator/metrics` or calls `MetricsCollector::snapshot` needs no change.

**Before (`{X.Y}`):**

```rust,ignore
MetricsSnapshot {
    http,
    idempotency,
    db_queries,
    read_your_writes_pins_total: 0,
}
```

**After (`{(X+1).0}`):**

```rust,ignore
// A literal does not compile. Get the snapshot from the collector.
let snapshot = state.metrics().snapshot();
// A destructure ends with `..`.
let MetricsSnapshot { db_queries, db_connection_holds, .. } = snapshot;
```

**Automation:** `manual` — a test that builds a snapshot by hand must record
into a `MetricsCollector` and call `snapshot()`. A codemod cannot write those
samples.

### Graph: `CrateGraph` has a `bins` field and is non-exhaustive

**Why:** a boundary package now also covers its renamed `[[bin]]` targets
(issue #3599). `CrateGraph::bins` holds the binary names of each package.

`CrateGraph` is now `#[non_exhaustive]`, so a later field does not break your
code.

**You are affected only if your code builds a `CrateGraph` with a struct
literal, or destructures one without `..`.** `autumn graph check-boundaries`
and `boundaries.toml` need no change.

**Before (`{X.Y}`):**

```rust,ignore
let crates = CrateGraph { deps, libs };
```

**After (`{(X+1).0}`):**

```rust,ignore
let mut crates = CrateGraph::default();
crates.deps = deps;
crates.libs = libs;
// A destructure ends with `..`.
let CrateGraph { deps, libs, .. } = crates;
```

**Automation:** `manual` — start from `CrateGraph::default()` and set the
fields.

---

### Features: the `cluster` module is optional

**Why:** `autumn_web::cluster` was 3% of the code that `autumn-web` compiles, in
every app (issue #2795, ADR 0045). It now has the `cluster` feature. The
feature is in `default`, so a default build does not change.

**You are affected only if your app sets `default-features = false` and uses
`autumn_web::cluster` or `[cluster] enabled = true`.** The compiler reports
``error[E0432]: unresolved import `autumn_web::cluster` ``. Without the
feature, `[cluster]` still parses, and `enabled = true` fails boot with a
validation error.

**Before (`{X.Y}`):**

```toml
autumn-web = { version = "{X.Y}", default-features = false, features = ["maud"] }
```

**After (`{(X+1).0}`):**

```toml
autumn-web = { version = "{(X+1).0}", default-features = false, features = ["maud", "cluster"] }
```

**Automation:** `manual` — a codemod cannot know if the app uses the
cluster. Add the feature only if it does.

---

### Features: failure capture needs `reporting`

**Why:** the `reporting` feature had two parts: the panic-catch layer and
failure capture (issue #2795, ADR 0046). The panic-catch layer now has its
own feature, `error-reporting`. `reporting` turns it on and adds failure
capture, so a manifest that names `reporting` builds the same code as
before. Both are in `default`.

**You are affected only if your app sets `default-features = false`, does not
name `reporting`, and sets `[failure_capture] enabled = true`.** Before,
that build did not know the section and wrote no capsule. Now the section
parses, and boot fails with a validation error that names the feature.

**Before (`{X.Y}`):**

```toml
autumn-web = { version = "{X.Y}", default-features = false, features = ["maud"] }
```

**After (`{(X+1).0}`):** add `"reporting"`, or set
`failure_capture.enabled = false`.

```toml
autumn-web = { version = "{(X+1).0}", default-features = false, features = ["maud", "reporting"] }
```

An app that wants the panic-catch layer and no failure capture names
`"error-reporting"`.

**Automation:** `manual` — a codemod cannot know if the app wants failure
capture.

---

### Features: `plugin-conformance`, `runtime-config` and `axum-ws`

**Why:** each app compiled the plugin conformance harness, the runtime-config
store and axum's WebSocket code, used or not (issue #2795, ADR 0049). Each
now has a feature. The three features are in `default`, so a default build
does not change.

**You are affected only if your app or plugin sets `default-features = false`
and uses one of these:**

| You use | The compiler reports | Add |
|---------|----------------------|-----|
| `autumn_web::plugin_conformance` | ``unresolved import `autumn_web::plugin_conformance` `` | `plugin-conformance` |
| `autumn_web::runtime_config`, `CostSignal::sync_from` | ``could not find `runtime_config` in `autumn_web` `` | `runtime-config` |
| axum's `extract::ws` or `extract::WebSocketUpgrade`, through `autumn_web::reexports::axum` or your own `axum` dependency | ``could not find `ws` in `extract` ``, or ``no `WebSocketUpgrade` in `axum::extract` `` | `axum-ws` |

`ws` turns `axum-ws` on, so an app that names `ws` does not change. An app
that turns `ws` on in its own `axum` dependency does not change. An app
that has its own `axum` dependency without `ws` adds `axum-ws` here, or
`ws` there: before, `autumn-web` turned `ws` on for it.
Without `runtime-config`, the cost signal does not follow the runtime-config
key. Set it from code with `CostSignal::set`.

**Before (`{X.Y}`):**

```toml
autumn-web = { version = "{X.Y}", default-features = false, features = ["maud"] }
```

**After (`{(X+1).0}`):** add only the features that the app uses.

```toml
autumn-web = { version = "{(X+1).0}", default-features = false, features = ["maud", "plugin-conformance", "runtime-config", "axum-ws"] }
```

**Automation:** `manual` — a codemod cannot know which of the three the app
uses.

---

### Config: a section is a table, not an array

**Why:** the config reader compiled code that reads each config struct from
an array, by field position (issue #2795, ADR 0049). No guide shows that
form. The reader now reads a struct from a table only, and `autumn-web`
compiles less code.

**You are affected only if `autumn.toml` gives the fields of a section as an
array.** Boot fails with
``invalid type: sequence, expected struct ServerConfig`` and the key,
``in `server` ``. An array of tables (`[[database.shards]]`) and an array
value (`allowed_origins = ["https://a.example"]`) do not change.

**Before (`{X.Y}`):**

```toml
server = [8080, "0.0.0.0"]
```

**After (`{(X+1).0}`):**

```toml
[server]
port = 8080
host = "0.0.0.0"
```

**Automation:** `manual` — give each field its name.

---

### Edge: handlers see only allowlisted request headers

**Why:** an identity-aware proxy adds a bearer assertion under its own header
name, for example `cf-access-jwt-assertion` or `x-goog-iap-jwt-assertion`.
The old denylist did not name these headers, so a capsule got them and could
echo them. Now only `autumn_edge::wire::ALLOWED_REQUEST_HEADERS` cross: content
negotiation, conditional requests, `cache-control` and `user-agent`.

**You are affected if** your code names `SENSITIVE_HEADERS` (from
`autumn_edge` or `autumn_edge::wire`) or `strip_sensitive_headers`, or an
`#[edge]` handler reads a header that is not on the list (for example `host`,
`x-request-id` or a custom header) through `HeaderMap`.
Both lanes now drop that header. The origin middleware still sees it.

**Before (`{X.Y}`):**

```rust
use autumn_edge::wire::{strip_sensitive_headers, SENSITIVE_HEADERS};

let safe = strip_sensitive_headers(&headers);
assert!(!SENSITIVE_HEADERS.contains(&"accept"));
```

**After (`{(X+1).0}`):**

```rust
use autumn_edge::wire::{allowed_request_headers, request_header_allowed};

let safe = allowed_request_headers(&headers);
assert!(request_header_allowed("accept"));
```

**Automation:** `manual` — a codemod cannot know which header a handler
needs. Move a handler that needs another header to an origin-only route.

## Plugin authors

**Required whenever this release changes the declared stable plugin surface**
(`autumn_web::plugin_contract::PLUGIN_SURFACES`) — `scripts/check-plugin-surface.sh`
fails a surface change that leaves this section empty or unwritten. Delete the
section entirely when the plugin surface did not change.

Everything here is addressed to someone maintaining an `autumn-plugin-*` /
`autumn-*-plugin` crate, not to an application author. See
[`docs/plugins.md`](../plugins.md#the-plugin-api-contract) for the tiers.

- **Stable surface changed:** {the API, what changed, and the replacement — or
  "none". Every entry here is a `**Breaking:**` change in the changelog and
  needs its own section under *Breaking changes* above.}
- **Experimental surface changed:** {the API and what changed — or "none".
  Experimental surface may change in any release, so this is a courtesy, not a
  promise. Plugins that declared it with `uses_experimental` are the audience.}
- **New stable surface (background tasks, #3545):**
  `AppBuilder::background_task`, `AppBuilder::background`,
  `AppBuilder::plugin_capability` and the `background` module
  (`BackgroundTask`, `PluginCapability`, `BackgroundCapability`,
  `Role::runs_plugin`). Move a loop that you `tokio::spawn` in `on_startup`
  to `background_task`, so that `AUTUMN_ROLE=web` does not run it. A plugin
  switch such as `worker_enabled` becomes `BackgroundTask::explicit`. A loop
  in `on_startup` that you do not move behaves as before. See
  [Background work in a plugin](../guide/extensibility.md#background-work-in-a-plugin)
  and [Background tasks for plugins and apps](../guide/jobs.md#background-tasks-for-plugins-and-apps).
- **In-repo plugins on background tasks (#3546):** `autumn-media-plugin`
  and `autumn-billing` start no loop from `on_startup` now. The reference
  plugin shows the pattern: one-time setup in `on_startup`, the loop in
  `background_task`. A test in `xtask` (`plugin_spawns`) fails on a spawn
  that an `on_startup` or `state_initializer` argument of a plugin crate
  reaches. `spawn_room_reaper_loop` and `spawn_retention_sweep_loop` are
  deprecated. Call `run_room_reaper` or `run_retention_sweep` from a
  `background_task`.
- **New experimental surface (fleet engines, #3642):** `db::fleet_engine`
  (the `FleetEngine` trait and `DatabaseFleet::with_engine`) and
  `plugin_conformance::fleet` (`FleetFixture`, `run_fleet_conformance`).
  Declare them with `uses_experimental`. An engine plugin implements
  `FleetEngine` and runs the kit in its own CI. `DatabaseFleet`,
  `FleetDatabase`, `FleetLifecycle` and `BackgroundOpen` take a parameter
  `E: FleetEngine`. With `sqlite`, `E` defaults to `SqliteEngine`, so
  `SQLite` code needs no change. A handler of another engine reads
  `FleetDatabase::handle`. See [ADR 0040](../adr/0040-fleet-engine-seams.md).
- **New experimental surface (fleet extractors):** the `fleet` module
  (`resolve`, `resolve_key`, `FleetLease`, `fleet_of`, `ownership_store`) and
  `AppBuilder::fleet_ownership_store`. Declare them with
  `uses_experimental`. An engine plugin writes its own extractor over
  `fleet::resolve::<E>` and puts its `DatabaseFleet<E>` in the app state with
  `insert_extension`. A sidecar hook ships through
  `FleetDatabase::sidecar_replica`. A `SlotOwnershipStore` gets a default
  `ensure_schema`, so an existing store needs no change. See
  [Writing a fleet extractor](../guide/sqlite-fleet.md#writing-a-fleet-extractor).
- **New stable surface (conformance, #2828):**
  `plugin_conformance::run_route_checks` and `RouteSource::from_wire`.
  `autumn plugin-check --plugin-name <your-plugin>` calls
  `run_route_checks`, so your conformance test and the CLI give the same
  result for the same routes.
- **Stable surface behavior (conformance, #2828):** `run_conformance` with a
  contract uses the contract's `registered_as` when no route carries the
  config name and a route carries `registered_as`. Routes under more than one name of the
  plugin fail `route-attribution`. No signature changes. A test that passed
  its crate name while the routes carry `Plugin::name()` now passes. Some
  CLI diagnostic text changes; do not match on it.
- **Stable surface behind a feature (conformance, #2795):**
  `plugin_conformance` has the `plugin-conformance` feature. The feature is
  in `default`. A plugin that depends on `autumn-web` with
  `default-features = false` and runs the harness in its tests names the
  feature. No signature changes.
- **Declared range to move to:** each release, bump the literal in
  `Plugin::contract`'s `.autumn_web("…")` to the new series (or write it with
  `lockstep_range(env!("CARGO_PKG_VERSION"))` if the plugin releases in
  lockstep with the framework) and re-run
  `autumn plugin-check --plugin-name <your-plugin>`. The examples in the
  `Plugin::contract` and `plugin_contract` rustdoc now name `"0.10"`, the
  series 0.10.0 opened; copy the shape, not the literal.

## Compiler error cheat sheet

Paste the most common errors a user will hit and the fix. This is the
single most valuable section of the guide — keep it factual and short.

| Error message (truncated) | Where you see it | Fix |
|---------------------------|------------------|-----|
| `error[E0432]: unresolved import \`autumn_web::foo\`` | module reorganized | `use autumn_web::<new path>;` |
| `error[E0061]: this function takes 2 arguments but 1 was supplied` | `App::run` added a parameter | see [Breaking changes › {Area}] |

## Configuration changes

- New key `[database.fleet] max_resident_bytes` (default `0`, no budget),
  and its variable `AUTUMN_DATABASE__FLEET__MAX_RESIDENT_BYTES`.

- New tables `[outbox.destinations.<name>]` (`url`, `signing_secret`,
  `topics`) and `[outbox.inbox]` (`enabled`, `signing_secret`,
  `previous_signing_secrets`, `timestamp_tolerance_secs`, `max_body_bytes`).
  A `signing_secret` of the form `"${NAME}"` reads the variable `NAME`.

- `autumn.toml` keys that were renamed, removed, or have new defaults.
- New `AUTUMN_*` environment variables.
- Default profile changes.

If nothing changed, delete this section.

## Behavior changes

- **Fleet:** an open under an ownership epoch refuses a database that
  records a newer epoch (`FleetError::NewerEpoch`, `421`). Before, the open
  served it. The check, the stamp and the write are one transaction. Only
  a host that lost its slot to a newer owner sees this. A control database
  made again from nothing restarts its epochs: re-import each database
  (`DatabaseFleet::import` drops the old stamp).
- **Fleet:** `/actuator/health` `db:fleet` details now include
  `resident_bytes` and `max_resident_bytes`.
- **Fleet:** the fleet core is in every `db` build, Postgres too. The `db`
  feature now pulls in `fd-lock`, which the `sqlite` feature pulled in
  before. No new crate.
- **Graph:** `autumn graph check-boundaries` checks the nodes of a renamed
  `[[bin]]` target in the boundary of its package (#3599). New violations
  can show. Fix them, or run with `--update-baseline`.
- **Config:** a type error in `autumn.toml` now names the full key, for
  example ``in `server.port` ``. Before, `AutumnConfig::load` showed a line
  of the merged config, which is not a file you wrote. `load_from` errors
  also give the key, not the line. A syntax error keeps its line. A message
  parser that read the `TOML parse error at line` prefix of a type error
  must read the key instead.
- **Capacity:** a value of the wrong type in `capacity.lock` now names the
  key, for example ``in `envelope.admission_limit` ``. Before, the error
  showed the line. A syntax error keeps its line.
- **Capacity:** a `capacity.lock` with an integer above 9223372036854775807
  does not load (`u64 value was too large`). That is the largest TOML
  integer. Only `seed` can be that large, and only from an explicit
  `autumn calibrate --seed`. Run `autumn calibrate` again with a smaller
  seed. `--seed` now refuses a larger value, and `autumn calibrate` does not
  write one.

Changes that still compile but behave differently at runtime. Examples:

- Error responses adopted a new JSON shape.
- A default middleware is now ordered differently.
- A scheduled task now runs on a different worker.

If nothing changed, delete this section.

- **Media and billing loops follow the role (#3546).** The `db` room reaper
  and the recording retention sweep do not run on `web`. The `memory` room
  reaper runs on `combined` and `web`, not on `worker`. The dunning re-arm
  runs on `combined` and `worker`, not on `web`. A fleet of only `web`
  replicas needs a `worker`, or `MediaPlugin::room_reaper_on_every_role()`
  and `MediaPlugin::retention_on_every_role()`. See
  [Media: background tasks and roles](../guide/media.md#background-tasks-and-roles).

### Comments: the comment router shows one page (#3325)

`commentable::router` reads one page of a thread, not the whole thread. A
page holds up to 50 top-level comments and all their replies. After a post,
the response shows the page that holds the new comment.

What to do:

1. On a host page that renders a thread, call `comment_thread_page` (not
   `comment_thread`), and pass the page to the widget with
   `.page(page.after, page.next)`.
2. Read `?after=<id>` from the host page's query string, and pass it to
   `comment_thread_page`.
3. Use the same page size on the host page and in
   `CommentsConfig::page_size`. Else a no-JS post can return to a page that
   does not show the new comment.
4. Index the comments table: `(commentable_type, commentable_id, created_at,
   id) WHERE parent_id IS NULL AND deleted_at IS NULL`, and `parent_id`.

### Metrics: the `"<route> SELECT"` key is deprecated

`db_queries["<route> SELECT"]` from the `Db` extractor is not a query. It is
the time a request held its connection, from checkout to release. It includes
all statements and all other work in the hold, for example a fan-out. A route
that only writes also shows it.

This release records each hold in two places:

| Key | Status |
| --- | --- |
| `db_queries["POST /census SELECT"]` | Deprecated for holds. The next release stops the `Db` extractor writing it. |
| `db_connection_holds["POST /census"]` | New. Use this one. |

`run_instrumented` also records a real `SELECT` under `"<route> SELECT"`.
In this release, that key can mix holds and statements. After the next
release, it holds only statements.

What to do:

1. Move each dashboard and alert that reads `db_queries["<route> SELECT"]`
   for the hold to `db_connection_holds["<route>"]`. The new key has no
   verb. A `Db` from a shard adds the shard, for example
   `"GET /notes shard=shard0"`.
2. Read statement times from `db_queries`. They come only from
   `autumn_web::db::run_instrumented`. If the app does not call it,
   `db_queries` is empty after the next release.

## Deprecations retained from `{X.Y}`

Items that were deprecated during the `{X.Y}` line and have now been
removed. Link each to the release where the deprecation notice first
appeared so users can see how much warning they had.

### Config-key removals

Config keys removed in this major release were registered in
`DEPRECATED_CONFIG_KEYS` (`autumn/src/config.rs`) with `remove_in = "{X+1}.0.0"`.
Startup issued a `WARN` log entry for each deprecated key detected in the config
(via `since = "{X.Y}"`), and `autumn doctor` surfaced them in the
`deprecated_keys` check.

For each removed config key, fill in the table below:

| Removed key (TOML / env var) | Replacement | Deprecated since | References |
|------------------------------|-------------|------------------|------------|
| `section.old_key` / `AUTUMN_SECTION__OLD_KEY` | `section.new_key` | `{X.Y}.0` | (link to changelog) |

If no config keys were removed, delete this subsection.

## Upstream dependency updates

For each major dependency bump carried with this release:

- Link to that project's upstream migration notes.
- Call out any of their changes that leak through Autumn's public API.

If no majors were carried, delete this section.

## How to verify

The reader's proof the upgrade landed. Keep it to concrete, checkable steps —
commands with expected output, not "make sure everything works". Required by
`scripts/check-migration-guides.sh`.

1. `cargo check` — clean, with none of the errors in the cheat sheet above.
2. `cargo test` — the suite is green on the new version.
3. `autumn doctor --strict` — no findings.
4. {one step per breaking change: the observable behaviour that proves the fix
   was applied, e.g. "hit `/x` and confirm the response carries `Y`"}

### Guide-only upgrade walkthrough

(The heading keeps its historical name; the walk-through itself is
codemod-first.) Upgrade an app scaffolded with `autumn new` on the **previous** release
**codemod-first** — `autumn upgrade` before any manual step — using only this
guide for what remains, and record the result here before publishing to
crates.io. See [`docs/release-checklist.md`](../release-checklist.md),
*Migration Guide Gate*.

- **Codemod:** {the `autumn upgrade` invocation the walk-through ran first, and
  what it covered. Required once this release ships any `auto`/`review`
  codemod; the remaining manual steps below must be only the `review`/`manual`
  changes.}
- **Status:** pending
  {the value must *begin* with `performed YYYY-MM-DD` once the walk-through is
  done, or `backfilled` for a guide written after its release shipped;
  `pending` is accepted only while this file is still `next.md`}
- **From → to:** `autumn-cli {X.Y.Z}` app upgraded to `autumn-web {X.Z.0}`
- **Elapsed:** {minutes — the budget is under 30 for a guide-only
  walk-through, and under 10 once `autumn upgrade` covers this release's
  rename-level changes (issue #1629)}
- **Gaps found and fixed in this guide:** {none, or what the walk-through
  exposed}

## Troubleshooting

Known rough edges, workarounds, and known-good version combinations
(e.g. "use `diesel 2.2.5+` — earlier `2.2.x` releases have a known
`pq-sys` linkage issue on macOS").

## Reporting problems

If you hit something not covered here, please open an issue at
<https://github.com/autumn-foundation/autumn/issues> with:

- The error message or unexpected behavior.
- The old version you upgraded from.
- A minimal reproduction if possible.

Migration guides are living documents — we update them based on user
reports for the first few months after a major release.
