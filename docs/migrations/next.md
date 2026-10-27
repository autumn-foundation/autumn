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

---

### Config: the `prod` profile enables `strict_config` and new protections

**Why:** The `prod` profile shipped with its protections off. A misspelled
timeout key took the default in silence. A slow database caused readiness to
flap across the fleet, instead of an early `503` (issue #3057).

**Before (`{X.Y}`):** with `AUTUMN_PROFILE=prod`, this booted, and
`statement_timeout` stayed unset:

```toml
[database]
statment_timeout = "5s"   # misspelled
```

**After (`{(X+1).0}`):** the same file stops the boot with "Strict config
check failed. Unknown keys in configuration". Correct the key, or turn the
check off:

```toml
[server]
strict_config = false     # or AUTUMN_SERVER__STRICT_CONFIG=false
```

The `prod` profile also changes these defaults. Each has a one-line opt-out:

| Default in `prod` | Opt-out |
| --- | --- |
| Load shedding at primary pool size × 32, at least 256 | `server.max_concurrent_requests = 0` |
| `database.statement_timeout = "30s"` | `statement_timeout = "0s"` |
| `database.idle_in_transaction_timeout = "60s"` | `idle_in_transaction_timeout = "0s"` |

Every profile also gets a migration `lock_timeout` of `5s` with `5` jittered
retries, in each transactional migration. Opt out with
`database.migration_lock_timeout = "0s"`. The `autumn migrate` CLI passes the
timeout to `diesel` in `PGOPTIONS`. Run migrations against Postgres directly,
not through a transaction pooler.

A long report query that runs inside a request now stops at `30s`. Give that
route a `StatementTimeout` extension, or raise the global value.

**Automation:** `manual` — this is a configuration and behaviour change, and no
code rewrite applies.

### Capacity: `AdmissionLimit` is `#[non_exhaustive]` and has a new variant

**Why:** The `prod` profile default ceiling needs its own source
(`AdmissionLimit::ProfileDefault`, issue #3057).

**Before (`{X.Y}`):**

```rust
match limit {
    AdmissionLimit::Configured(n) | AdmissionLimit::Contract(n) => Some(n),
    AdmissionLimit::Unlimited => None,
}
```

**After (`{(X+1).0}`):** use `limit.limit()`, or add a wildcard arm:

```rust
let ceiling: Option<usize> = limit.limit();
```

**Automation:** `manual` — a new enum variant needs a new match arm, and no
safe rewrite can choose its body.

### Config: `HealthConfig` gains four public fields

**Why:** `/ready` now pings the primary database. The new fields set the
ping cache, the ping time limit, and which pings gate `/ready` (issue #3059).

**Before (`{X.Y}`):** a struct literal listed every field.

```rust
let health = autumn_web::config::HealthConfig {
    enabled: true,
    path: "/health".into(),
    live_path: "/live".into(),
    ready_path: "/ready".into(),
    startup_path: "/startup".into(),
    detailed: false,
};
```

**After (`{(X+1).0}`):** add `..HealthConfig::default()`. It sets
`cache_ttl_ms = 1000`, `ping_timeout_ms = 2000`, `db_readiness = true` and
`redis_readiness = false`.

```rust
let health = autumn_web::config::HealthConfig {
    detailed: false,
    ..autumn_web::config::HealthConfig::default()
};
```

**Automation:** `manual` - the fix adds a struct update expression, and no
codemod rewrites struct literals.

### Config: `TenancyConfig`, `JobConfig` and `JobPostgresConfig` have new fields

**Why:** per-tenant bulkheads, tenant job lanes and shard-local jobs (issue
#3072). `TenancyConfig` gains `max_concurrent_requests` and
`max_db_connections`. `JobConfig` gains `tenants`. `JobPostgresConfig` gains
`shard_local`.

**Before (`{X.Y}`):** a struct literal listed every field.

```rust
let postgres = autumn_web::config::JobPostgresConfig {
    visibility_timeout_ms: 30_000,
};
```

**After (`{(X+1).0}`):** add `..Default::default()`. Every new field is off
by default (`0` or `false`).

```rust
let postgres = autumn_web::config::JobPostgresConfig {
    visibility_timeout_ms: 30_000,
    ..autumn_web::config::JobPostgresConfig::default()
};
```

**Automation:** `manual` - the fix adds a struct update expression, and no
codemod rewrites struct literals.

### Tenancy: `TenantPropagatingBody` has a `db_bulkhead` field

**Why:** a database checkout while a streaming body is polled counts against
`tenancy.max_db_connections` (issue #3072).

**Before (`{X.Y}`):**

```rust
let body = TenantPropagatingBody {
    inner,
    tenant_id,
    handle: None,
};
```

**After (`{(X+1).0}`):** add `db_bulkhead: None`.

```rust
let body = TenantPropagatingBody {
    inner,
    tenant_id,
    handle: None,
    db_bulkhead: None,
};
```

**Automation:** `manual` - no codemod rewrites struct literals.

### Probes: `/ready` pings the primary database

**Why:** pool saturation made a busy replica unready, and an idle pool made a
dead database look ready (issue #3059).

**Before (`{X.Y}`):** `/ready`, `/health` and the `db` component of
`/actuator/health` failed when the pool had no free connection and a request
waited. They did not connect to the primary.

**After (`{(X+1).0}`):** they send `SELECT 1` to the primary on one dedicated
connection. A failed ping, or one slower than `health.ping_timeout_ms`, gives
`503`. A busy pool does not. Do these checks:

- Add one connection per replica to your Postgres `max_connections` budget.
- Keep `health.ping_timeout_ms` below the probe timeout of your platform (for
  Kubernetes, `timeoutSeconds`).
- To keep a replica in rotation when the primary fails, set
  `health.db_readiness = false`.
- The generated Dockerfile `HEALTHCHECK` probes `/health`. It now fails when
  the primary is down. ECS and Docker Swarm replace an unhealthy container. On
  those platforms, set `AUTUMN_HEALTHCHECK_URL=http://localhost:3000/live`.
- A test that changes a health indicator and reads `/actuator/health` again
  in less than 1 s can read the cached result. Set `health.cache_ttl_ms = 0`
  in the test config.

**Automation:** `manual` - it is a runtime behaviour change, and no code
rewrite applies.

### Resilience: `CircuitBreakerPolicy` and `CircuitBreakerPolicyConfig` have slow-call fields

**Why:** The breaker opened on failures only. A dependency that became slow
but did not fail did not open it (issue #3060).

**Before (`{X.Y}`):**

```rust
let policy = CircuitBreakerPolicy {
    failure_ratio_threshold: 0.5,
    sample_window: Duration::from_secs(10),
    minimum_sample_count: 10,
    open_duration: Duration::from_secs(60),
    half_open_trial_count: 3,
};
```

**After (`{(X+1).0}`):**

```rust
let policy = CircuitBreakerPolicy {
    failure_ratio_threshold: 0.5,
    sample_window: Duration::from_secs(10),
    minimum_sample_count: 10,
    open_duration: Duration::from_secs(60),
    half_open_trial_count: 3,
    ..CircuitBreakerPolicy::default()
};
```

The defaults are a 60 s slow-call threshold, a slow-call rate threshold of
`1.0`, and `CancelledCallOutcome::Slow`. Set `slow_call_duration_threshold:
None` to keep the old behaviour.

A struct literal of `autumn_web::config::CircuitBreakerPolicyConfig` needs
`..Default::default()` for the same reason.

**Automation:** `manual` - each struct literal needs a value for the new
fields, and the choice changes when the breaker opens.

### Media: `MediaPlugin` installs only the primitives you enable

**Why:** The docs said both primitives are off by default, but `build`
installed storage, the encode jobs and the retention sweep for every plugin.
`with_broadcast()` did nothing. Issue #1974.

**Before (`{X.Y}`):**

```rust
// Storage, encode jobs and the retention sweep installed.
autumn_web::app().plugin(MediaPlugin::new().config(media).recordings_root("recordings"))
```

**After (`{(X+1).0}`):**

```rust
// Enable the primitive you use. Broadcast also installs MediaMtxClient and MediaUrls.
autumn_web::app().plugin(
    MediaPlugin::new()
        .config(media)
        .with_broadcast()
        .recordings_root("recordings"),
)
```

With no primitive, the plugin installs no routes, extensions or jobs, and logs
a warning. `extension::<MediaWorkflows>()` then returns `None`, jobs on the
`media` queue have no handler, and the retention sweep does not start.

**Automation:** `manual` — this is a runtime behavior change. The code still
compiles, so a codemod cannot know which primitive your app uses.

### Media: `RoomStore::heartbeat` and `RoomStore::roster` take a `session_max` limit

**Why:** A heartbeat renewed the room token with no limit, and a roster poll
accepted a token of any age. So a captured token stayed valid for as long as
someone used it. Now neither works past `joined_at + session_max`. Issue #1974.

**Before (`{X.Y}`):**

```rust,ignore
fn heartbeat<'a>(
    &'a self,
    namespace: &'a str,
    room_id: &'a str,
    participant_id: &'a str,
    token: &'a str,
    token_ttl: Duration,
) -> RoomStoreFuture<'a, DateTime<Utc>>;
```

**After (`{(X+1).0}`):**

```rust,ignore
fn heartbeat<'a>(
    &'a self,
    namespace: &'a str,
    room_id: &'a str,
    participant_id: &'a str,
    token: &'a str,
    token_ttl: Duration,
    session_max: Duration,
) -> RoomStoreFuture<'a, DateTime<Utc>>;
```

`roster` gains the same last argument:
`fn roster<'a>(&'a self, namespace: &'a str, room_id: &'a str, auth_token: &'a str, session_max: Duration)`.

In an out-of-tree store, compute the new expiry with
`autumn_media_plugin::renewed_expiry(joined_at, now, token_ttl, session_max)`.
When it returns `None`, return `RoomError::RoomNotFound` and change nothing. In
`roster`, refuse a matching member when `now >= joined_at + session_max`, and
do not refresh its `last_seen_at`.

`MediaConfig` also has two new public fields, `room_session_max_seconds`
(default `43200`) and `room_rate_limit_per_minute` (default `0`, off). A
struct literal that lists every field must add them, or use
`..MediaConfig::default()`. With rooms enabled, boot fails when
`room_session_max_seconds` is less than `room_token_ttl_seconds`.

A client that stays in a room for more than 12 hours now gets `404` from
heartbeat and roster. It must leave, then join again. To keep longer sessions,
set a larger `room_session_max_seconds`.

**Automation:** `manual` — a codemod cannot write your store's renewal logic.

### Feature flags: `PgFlagStore::get` errors before the first load

**Why:** `get` connected to the database on the request thread, and a store
error turned every flag off (issue #3063). Now `get` reads an in-memory
snapshot and never connects on a Tokio worker.

**Before (`{X.Y}`):** `get` on a new store read the database. A read error
looked like an absent flag to code that used `.ok().flatten()`:

```rust
let store = PgFlagStore::new(url);
if store.get("beta").ok().flatten().is_none() {
    store.disable("beta", Some("init")).ok(); // also ran on a read error
}
```

**After (`{(X+1).0}`):** on a Tokio runtime, `get` returns an error until the
first load ends. Load first, and seed only on `Ok(None)`:

```rust
let store = PgFlagStore::new(url);
store.refresh()?; // blocks: call it at startup
if matches!(store.get("beta"), Ok(None)) {
    store.disable("beta", Some("init"))?;
}
```

An app that registers the store with `with_flag_store` needs no change: the
app loads the store at startup. `with_cache_ttl(url, Duration::ZERO)` now
means "refresh at each read", not "read the database at each read".

**Automation:** `manual` - it is a behaviour change, and no code rewrite
applies.

### Agent authority: the analyser refuses more handler shapes

**Why:** The `#[agent_operable]` analyser let some handler shapes through
with no proof (issue #2451). It now refuses them. Each refusal names the call
site and the `#[agent_effect(...)]` that resolves it.

**Before (`{X.Y}`):** these compiled. A module-level `use tokio::time::sleep;`
made a bare `sleep` look inert, a binding named `session` or `cache` was
trusted by name, and an opaque macro with no handle in its tokens passed:

```rust
#[agent_operable(grant = RefundDrafter)]
async fn drafts(session: Session, d: Duration) -> R {
    sleep(d).await;
    session.refresh().await?;
    schedule!();
    Ok(())
}
```

**After (`{(X+1).0}`):** write the `tokio` path, type the parameter, and
declare the opaque macro:

```rust
#[agent_operable(grant = RefundDrafter)]
async fn drafts(session: Session, d: Duration) -> R {
    tokio::time::sleep(d).await;
    session.refresh().await?; // `Session` is the type that makes it inert
    #[agent_effect(jobs(NotifyFinance), reason = "schedule! enqueues it")]
    schedule!();
    Ok(())
}
```

Other new refusals: an awaited callee that is not a path, an unread future
held in a tuple, block or pattern, a `diesel::` write whose table is not
`<table>::table`, and any macro in expression position that is not a plain
value macro (`format!`, `vec!`, `json!`). `tx.commit()` is inert only on a
transaction from `db.begin()` or a `tx` callback.

**Automation:** `manual` - each refusal needs a choice that only the author
can make.

### Config: `HttpClientConfig` and `RequestTimeoutsConfig` get new fields

**Why:** Deadline propagation and the retry budget (issue #3058) need
settings. Neither struct is `#[non_exhaustive]`, so a struct literal stops
compiling.

**Before (`{X.Y}`):**

```rust,ignore
let client = autumn_web::config::HttpClientConfig {
    timeout_secs: 10,
    max_retries: 1,
    max_retry_after_secs: 10,
    base_urls: std::collections::HashMap::new(),
};
```

**After (`{(X+1).0}`):**

```rust,ignore
let client = autumn_web::config::HttpClientConfig {
    timeout_secs: 10,
    max_retries: 1,
    ..autumn_web::config::HttpClientConfig::default()
};
```

Do the same for `RequestTimeoutsConfig`, which gets `accept_deadline_header`.

**Automation:** `manual` - the fix is a `..Default::default()` tail, which no
codemod adds.

---

### Ledger: raw-SQL framework writes to a ledgered table are refused (#2319)

**Why:** a counter-cache update or a `delete_all` / `nullify` cascade runs raw
SQL. On a ledgered table it changed rows and recorded no revision, so the
ledger disagreed with the table.

**Before (`{X.Y}`):** the write ran. `ledger_verify` later reported
`LiveStateMismatch`:

```rust
#[repository(Post, soft_delete, ledgered = true)]
pub trait PostRepository {}
// Comment has #[belongs_to(Post, counter_cache)]: each comment bumped posts.comment_count.
```

**After (`{(X+1).0}`):** the write fails with `LedgerError::OutOfBandWrite`
(HTTP 409). Remove the counter cache, or the `dependent(...)` clause. For a
cascade, use `on_delete = destroy`: a ledgered child records a revision.

**Automation:** `manual` - it is a behaviour change, and no code rewrite
applies.

---

### Ledger: more writes to a ledgered table are refused (#2319)

**Why:** these framework paths write raw SQL and record no revision, so the
ledger did not match the table. A NULL tenant made a chain that no read could
reach. A tenant move split a chain in two.

**Before (`{X.Y}`):** each of these compiled and ran:

```rust
#[repository(Post, soft_delete, ledgered = true)]
pub trait PostRepository {
    fn delete_by_title(title: String) -> ();   // bulk delete, no revision
}
// `Note` is `tenant_scoped` and ledgered, with `tenant_id: Option<String>`.
// `Post` is `#[votable]`; `react()` updated `posts.vote_score`.
```

**After (`{(X+1).0}`):**

| Path | Result |
|------|--------|
| `#[votable]` `react()` on a ledgered target or edge table | `OutOfBandWrite` (HTTP 409) |
| `add_comment` / `delete_comment` into a ledgered comments table | `OutOfBandWrite` |
| `add_*` / `remove_*` / `set_*` into a ledgered `has_many(through)` join table | `OutOfBandWrite` |
| A capsule import into a ledgered table | `DataCapsuleError::InvalidInput` |
| A write from an unledgered repository on a ledgered table, including `with_lock`, `find_or_create_by_*` and a `destroy` cascade through it | `OutOfBandWrite` |
| A model factory `create()` into a ledgered table | Panic |
| An update that changes a ledgered record's `tenant_id` | `TenantChange` (HTTP 409) |
| A derived `delete_by_*` on a ledgered repository | Compile error |
| `Option<String>` tenant column on a ledgered `tenant_scoped` repository | Compile error |

To fix: find the records, then call `delete_by_id` or `delete_many`. Make the
tenant field `String` and the column `NOT NULL`. Write through the ledgered
repository. Or stop ledgering the table.

**Automation:** `manual` - each fix depends on the app's data model.

---

### Shadow: `ShadowStats` gains `comparisons_abandoned`

**Why:** the mirror deadline now covers the comparison. A comparison that does
not finish is counted apart from a match, a divergence and a skip (issue #2333).

**Before (`{X.Y}`):** a struct literal listed every field.

```rust
let stats = autumn_web::shadow::ShadowStats {
    mirrored: 0,
    compared: 0,
    // ... every other field ...
    primary_incomplete: 0,
};
```

**After (`{(X+1).0}`):** add `..ShadowStats::default()`. An exhaustive
destructuring pattern needs `..` or the new field.

```rust
let stats = autumn_web::shadow::ShadowStats {
    mirrored: 1,
    ..autumn_web::shadow::ShadowStats::default()
};
```

**Automation:** `manual` - the fix adds a struct update expression, and no
codemod rewrites struct literals.

### Capsules: new `CacheEffect` and `EffectSeam` variants, new `JobEffect` and `CapsuleEffects` fields

**Why:** issue #2351 adds cache removals and random-draw widths to the capsule
seam. It also records the deadline of an `enqueue_at` call when the deadline
is already past, and the HTTP status of a failed enqueue. These types (feature `reporting`) are public and are not
`#[non_exhaustive]`.

**Before (`{X.Y}`):**

```rust
use autumn_web::capsule::{CacheEffect, JobEffect};

fn key(effect: &CacheEffect) -> &str {
    match effect {
        CacheEffect::Get { key, .. } | CacheEffect::Insert { key, .. } => key,
    }
}

let job = JobEffect {
    name: "send_receipt".to_owned(),
    payload: serde_json::json!({}),
    delay_secs: None,
    due_at: None,
    error: None,
};
```

**After (`{(X+1).0}`):** match the new variants, or call `CacheEffect::key`.
Add `..JobEffect::default()` to a struct literal.

```rust
use autumn_web::capsule::{CacheEffect, JobEffect};

fn key(effect: &CacheEffect) -> &str {
    effect.key()
}

let job = JobEffect {
    name: "send_receipt".to_owned(),
    payload: serde_json::json!({}),
    ..JobEffect::default()
};
```

The new variants are `CacheEffect::Invalidate`, `CacheEffect::InvalidateNamespace`,
`CacheEffect::Clear`, `EffectSeam::Random`, `EffectSeam::Detached` and
`MailErrorKind::NoDurableQueueInProduction`. An exhaustive `match` on
`EffectSeam` or `MailErrorKind` needs a new arm.

`CapsuleEffects` has a new field `state_cache`. A struct literal of
`CapsuleEffects` needs `..CapsuleEffects::default()`.

`CAPSULE_FORMAT_VERSION` is now 4. An older build refuses a version 4 capsule
with a version mismatch. This build reads a version 3 capsule and replays it
with the version 3 rules, so a committed corpus keeps its verdicts. Re-record
a capsule to get the new checks.

**Automation:** `manual` - a codemod cannot know what a new match arm must do.

---

### MCP: `ApiDoc` gains a `mcp_task` field

**Why:** MCP task tools (`#[api_doc(mcp, task)]`) need a flag on the route
metadata. `ApiDoc` has public fields and is not `#[non_exhaustive]`, so a new
field breaks a struct literal. The route macros set the field for you.

**Before (`{X.Y}`):**

```rust
use autumn_web::openapi::ApiDoc;

let doc = ApiDoc {
    method: "GET",
    path: "/api/report",
    // …every other field spelled out…
};
```

**After (`{(X+1).0}`):**

```rust
use autumn_web::openapi::ApiDoc;

let doc = ApiDoc {
    method: "GET",
    path: "/api/report",
    ..ApiDoc::default()
};
```

The new field is `pub mcp_task: bool`. It is `false` by default, so a route
that does not opt in behaves as before.

**Automation:** `manual` — a codemod cannot know which fields a struct literal
means to leave at their defaults.

### MCP: `ApiDoc` gains a `mcp_core` field

**Why:** a core MCP tool (`#[api_doc(mcp, core)]`) needs a flag on the route
metadata. A new public field breaks a struct literal, as for `mcp_task`. The
route macros set the field for you.

**Before (`{X.Y}`):**

```rust
use autumn_web::openapi::ApiDoc;

let doc = ApiDoc {
    method: "GET",
    path: "/api/todos",
    // …every other field spelled out…
};
```

**After (`{(X+1).0}`):**

```rust
use autumn_web::openapi::ApiDoc;

let doc = ApiDoc {
    method: "GET",
    path: "/api/todos",
    ..ApiDoc::default()
};
```

The new field is `pub mcp_core: bool`. It is `false` by default, so a route
that does not set it behaves as before.

**Automation:** `manual` — a codemod cannot know which fields a struct literal
means to leave at their defaults.

### WebMCP: `ApiDoc` gains `webmcp_tool` and `webmcp_exclude` fields

**Why:** a WebMCP tool (`#[api_doc(webmcp)]`, issue #2619) needs flags on
the route metadata. A new public field breaks a struct literal, as for
`mcp_task`. The route macros set the fields for you.

**Before (`{X.Y}`):**

```rust
use autumn_web::openapi::ApiDoc;

let doc = ApiDoc {
    method: "GET",
    path: "/api/todos",
    // …every other field spelled out…
};
```

**After (`{(X+1).0}`):**

```rust
use autumn_web::openapi::ApiDoc;

let doc = ApiDoc {
    method: "GET",
    path: "/api/todos",
    ..ApiDoc::default()
};
```

Both new fields are `bool` and `false` by default. A route that does not opt
in behaves as before.

**Automation:** `manual` — a codemod cannot know which fields a struct literal
means to leave at their defaults.

### Agent authority: manifest rows gain WebMCP fields

**Why:** the agent-authority manifest reports WebMCP exposure apart from
`/mcp` exposure (issue #2619). `RouteSummary` gains `webmcp_tool` and
`webmcp_exposed_by`. `RouteRef` and `UnregisteredAuthority` gain
`webmcp_tool`. `AgentAuthorityManifest` gains `ungoverned_webmcp_tools`.

This affects only code that builds these structs by hand. The JSON does not
change for an app with no WebMCP tool. The JSON omits each new field when it
is `false` or empty, so a committed manifest does not drift.

**Before (`{X.Y}`):**

```rust
let route = RouteSummary {
    // …
    duplicate_operation_id: false,
    agent_authority: None,
};
```

**After (`{(X+1).0}`):**

```rust
let route = RouteSummary {
    // …
    duplicate_operation_id: false,
    webmcp_tool: false,
    webmcp_exposed_by: None,
    agent_authority: None,
};
```

**Automation:** `manual` — add the fields with `false`, `None`, or an empty
`Vec`.

### `StaticRouteMeta` has a `content_type` field

**Why:** `#[static_get]` now takes `content_type = "..."` (#2403). The macro
passes the value in this field.

This affects only code that builds `StaticRouteMeta { .. }` by hand. Routes
declared with `#[static_get]` do not change.

**Before (`0.8`):**

```rust
let meta = StaticRouteMeta { path: "/a", name: "a", revalidate: None, params_fn: None, seo };
```

**After (next release):**

```rust
let meta = StaticRouteMeta { path: "/a", name: "a", revalidate: None, params_fn: None, seo, content_type: None };
```

**Automation:** `manual` — add `content_type: None` to each literal.

### Data flow: manifest rows have a crate version in `model_path`

**Why:** Two incompatible versions of one crate can link into one binary. They
had the same `model_path`, so `autumn data-flow` merged their rows (issue
#2375).

**Before (`{X.Y}`):**

```json
"model_path": "shop::models::customer::Customer"
```

**After (`{(X+1).0}`):** the path has the compatible version of the defining
crate. This is the major version. For a crate before 1.0 it is `0.minor`. For a
crate before 0.1 it is `0.0.patch`. A prerelease is the full version. Other patch releases do not change it.

```json
"model_path": "shop::models::customer@2::Customer"
```

Make the manifest again. Commit it:

```sh
autumn data-flow --manifest data-flow-manifest.json
```

Until you do, `autumn data-flow --check` reports drift for each row. The
`model` name is not changed. The report shows the full path only when two rows
have the same `model` name.

The path changes again when the major version of your own crate changes (or the
minor version, before 1.0). Make the manifest again after such a release.

**Automation:** `manual` - make the manifest again with the command above.

### Repository: dependent cascades probe first and `delete_many` hooks parents first

**Why:** A dependent cascade must fire no hook when a `restrict` blocks it, and
`delete_by_id` and `delete_many` must fire hooks in one order (issue #2427).
The public `DependentAction` enum gains a hidden `Probe` variant, and
`RuntimeDependentSpec` gains a `probe` field.

**Before (`0.8`):**

- A `restrict` below a `destroy` leg could answer 409 after earlier hooks fired.
- `delete_many` fired child hooks before the parents' `before_delete`.

**After (`0.9`):**

- Every `restrict` in the tree is probed before any hook fires.
- `delete_many` fires every parent's `before_delete` first, as `delete_by_id` does.
- The probe reads each `destroy` child set once more, and locks its rows.

Most apps need no change. Two cases need work:

- A `before_delete` hook that relied on a child hook firing first. Move that
  logic to the child hook.
- Code that matches `DependentAction` exhaustively, or builds a
  `RuntimeDependentSpec` by hand. Add a wildcard arm, and set `probe: None`.
  Both types are framework plumbing.

**Automation:** `manual` — a hook-order dependency needs a human decision.

### Cache coherence: `MutationDescriptor` and `Mutation` have new fields (#2364)

**Why:** The cache audit must see the writes a nested `dependent(...)` cascade
makes below its direct child. A repository names only its own children, so each
cascade write now names the child repository it runs. The audit follows those
links.

**Before (`0.8`):**

- `MutationDescriptor` had no `cascades_into` field.
- `Mutation` had no `cascades_into` or `cascade_recurses` field.
- A root delete registered a write for the direct child model only.

**After (`0.9`):**

- A root delete registers a write for each model below it in a chain of
  `destroy` cascades. `nullify` and `delete_all` stop the chain.
- The root repository must declare an edge for each such model. An app that
  had no edge for a grandchild read now fails `autumn cache audit`.

Most apps need no change. Two cases need work:

- A cached read of a grandchild model that the audit now flags. Add
  `invalidates(...)` to the root repository, or `acknowledge_stale = "reason"`.
- Code that builds `MutationDescriptor` or `Mutation` by hand. Set
  `cascades_into: None` on both, and `cascade_recurses: false` on `Mutation`.
  The `#[repository]` macro does this for you.

**Automation:** `manual` — a flagged read needs a human decision.

### Idempotency: `RouteIdempotency` has an `Off` variant

**Why:** A route can now opt out of `.idempotent()` with
`#[post("/x", idempotent = false)]`, for a route that dedupes by its own key
(a durable workflow route, say). The macro records that as
`RouteIdempotency::Off`. `RouteIdempotency` is not `#[non_exhaustive]`, so a
new variant breaks an exhaustive `match`.

**Before (`{X.Y}`):**

```rust
match route.idempotency {
    RouteIdempotency::Direct => "fail closed",
    RouteIdempotency::ReplayThroughInner => "replay",
}
```

**After (`{(X+1).0}`):**

```rust
match route.idempotency {
    RouteIdempotency::Direct => "fail closed",
    RouteIdempotency::ReplayThroughInner => "replay",
    RouteIdempotency::Off => "not cached",
}
```

Code that only builds a `Route` with `Direct` or `ReplayThroughInner`, or
that tests with `matches!`, needs no change.

**Automation:** `manual` — a new enum variant needs a new match arm, and no
safe rewrite can choose its body.

### Idempotency: bearer-token requests get their own cache slot

**Why:** A request with no cookie-backed session used to compute a storage key
with no principal in it, so two token clients that sent the same
`Idempotency-Key` and body to a route shared one cached response. The key now
carries the principal `RequireApiToken` verified, or a SHA-256 digest of the
raw `Authorization` header when the route checks the token itself. See
[the idempotency guide](../guide/idempotency.md#bearer-token-clients).

**Before (`{X.Y}`):**

- Client B with client A's key and body was served A's stored response.
- A retry with the `Authorization` header changed or dropped replayed.
- `RequireApiToken` on `AppBuilder::layer` forced fail-closed replay app-wide:
  every cache hit, a same-client retry included, answered `409`.

**After (`{(X+1).0}`):**

- Each bearer client replays only its own response.
- A retry with a different raw `Authorization` header (on a route without
  `RequireApiToken`) runs the handler again.
- `RequireApiToken` registered directly with `AppBuilder::layer` or
  `static_gate` no longer forces fail-closed replay: a retry replays. A
  `ServiceBuilder` stack around it still fails closed.

Most apps need no change. Two cases need a look:

- A request in flight across the upgrade that carries `Authorization` and no
  session gets a new key, so its retry runs the handler once more. Session-only
  apps keep byte-identical keys.
- A client that refreshes its token between retries, on a route that checks the
  token in the handler, now runs the handler again. Mount `RequireApiToken` with
  `AppBuilder::layer` to key on the principal instead of the token.

**Automation:** `manual` — whether a retry may run twice is the app's call.

### Graph: `served()` takes the app's `AppState`

**Why:** The served architecture graph was process-global. A second application
in one process served the graph of the first, with its paths and auth (issue
#2520). Each application now keeps its own graph in its `AppState`.

**Before (`{X.Y}`):**

```rust
let graph: Option<&'static ArchitectureGraph> = autumn_web::graph::served();
```

**After (`{(X+1).0}`):**

```rust
let graph: Option<Arc<ArchitectureGraph>> = autumn_web::graph::served(&state);
```

A custom `ProvideActuatorState` gets a new `architecture_graph_json` method. Its
default returns `None`, so `/actuator/graph` answers `503` for that state.
`AppState` overrides it.

The graph descriptor types in `autumn_web::graph` have a new `crate_version`
field. The macros fill it. Do not build these types by hand.

**Automation:** `manual` — pass the state to `served`.

### Billing: `Write` has a `Tie` variant

**Why:** The reconciler read the stored subscription before the guarded write
to find a same-second tie. Two webhooks at the same time could both miss it
(issue #3111). The store now reports the tie inside the write.

**Before (`{X.Y}`):**

```rust
match store.upsert_subscription(upsert).await? {
    Write::Applied(row) | Write::Unchanged(row) | Write::Stale(row) => row,
}
```

**After (`{(X+1).0}`):**

```rust
match store.upsert_subscription(upsert).await? {
    Write::Applied(row) | Write::Unchanged(row) | Write::Stale(row) | Write::Tie(row) => row,
    _ => unreachable!("Write is non-exhaustive"),
}
```

`Write` is `#[non_exhaustive]`, so a match needs a wildcard arm. Prefer
`Write::into_inner` when you only need the row.

A custom `BillingStore` must handle `SubscriptionUpsert::check_ties` inside
`upsert_subscription`. Return `Write::Tie(stored_row)` and write nothing when
all of these are true: the flag is set, the statuses differ, neither status is
terminal, the instants are equal, and the write is not `authoritative`. Make
the check and the write one atomic step. A store that never returns `Tie` still
compiles. It ranks concurrent same-second webhooks as before. Only subscription
writes return `Tie`.

**Automation:** `manual` — add the arm, and the check to a custom store.

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
- **New stable surface:** {additive APIs plugin authors can now rely on — or
  "none". These are not breaking; they are why a plugin author reads this
  section for the good news too.}
- **Declared range to move to:** each release, bump the literal in
  `Plugin::contract`'s `.autumn_web("…")` to the new series (or write it with
  `lockstep_range(env!("CARGO_PKG_VERSION"))` if the plugin releases in
  lockstep with the framework) and re-run
  `autumn plugin-check --plugin-name <your-plugin>`. The examples in the
  `Plugin::contract` and `plugin_contract` rustdoc now name `"0.9"`, the
  series 0.9.0 opened; copy the shape, not the literal.

## Compiler error cheat sheet

Paste the most common errors a user will hit and the fix. This is the
single most valuable section of the guide — keep it factual and short.

| Error message (truncated) | Where you see it | Fix |
|---------------------------|------------------|-----|
| `error[E0432]: unresolved import \`autumn_web::foo\`` | module reorganized | `use autumn_web::<new path>;` |
| `error[E0061]: this function takes 2 arguments but 1 was supplied` | `App::run` added a parameter | see [Breaking changes › {Area}] |

## Configuration changes

- `autumn.toml` keys that were renamed, removed, or have new defaults.
- New `AUTUMN_*` environment variables.
- Default profile changes.

If nothing changed, delete this section.

## Behavior changes

Changes that still compile but behave differently at runtime. Examples:

- Error responses adopted a new JSON shape.
- A default middleware is now ordered differently.
- A scheduled task now runs on a different worker.

If nothing changed, delete this section.

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
