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

### Commentable: the runtime helpers take `soft_delete: Option<bool>`

**Why:** a model can have a `soft_delete` repository and a plain one. The
helpers did not know the calling repository. Thus they returned `404` for a
soft-deleted parent through both (#2284). The caller now gives the fact.

This affects only direct calls to the `autumn_web::commentable` functions. The
generated `{Model}Comments` methods (`repo.add_comment(...)` and so on) do not
change. The generic router does not change.

**Before (`0.8`):**

```rust
use autumn_web::commentable::comment_thread;

let thread = comment_thread(&mut conn, Post::commentable_spec(), "Post", id, None).await?;
```

**After (next release):**

```rust
use autumn_web::commentable::comment_thread;

// `None`: the old behavior. Hide the parent if any repository soft-deletes.
// `Some(true)` / `Some(false)`: your repository's own `soft_delete` setting.
let thread =
    comment_thread(&mut conn, Post::commentable_spec(), "Post", id, None, None).await?;
```

Do the same for `add_comment`, `delete_comment` and
`recompute_comment_count`: add `None` as the last argument.

**Automation:** `manual` — the new argument depends on the calling
repository. A codemod cannot know that repository.

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

### Jobs: `JobInfo` has a new `timeout` field

**Why:** A job can now set the longest time one run may take (issue #3051).
`JobInfo` carries it as `timeout: Option<Duration>`.

**Before (`0.8`):**

```rust
let info = JobInfo {
    name: "export".to_string(),
    max_attempts: 3,
    initial_backoff_ms: 250,
    queue: "default".to_string(),
    uniqueness: None,
    concurrency: None,
    version: 1,
    handler: export_handler,
};
```

**After (`0.9`):**

```rust
let info = JobInfo {
    name: "export".to_string(),
    max_attempts: 3,
    initial_backoff_ms: 250,
    queue: "default".to_string(),
    uniqueness: None,
    concurrency: None,
    version: 1,
    timeout: None, // or Some(Duration::from_secs(30))
    handler: export_handler,
};
```

Code that uses `#[job]` or `JobInfo::new` does not change. `JobConfig` also
has a new `default_timeout_ms` field: see the struct-literal note in the next
section.

**Automation:** `manual` — add `timeout: None` to each hand-written `JobInfo`
literal. `#[job]` and `JobInfo::new` set it.

### Config: `AutumnConfig` gains a `cost` field

**Why:** Per-request cost records and cost-aware deferral (issue #1720) need
their own `[cost]` section. `AutumnConfig` has public fields and is not
`#[non_exhaustive]`, so a new field breaks a struct literal. `Default` and
`..AutumnConfig::default()` keep working.

**Before (`{X.Y}`):**

```rust
use autumn_web::config::AutumnConfig;

let config = AutumnConfig {
    server: my_server_config,
    // …every other field spelled out…
};
```

**After (`{(X+1).0}`):**

```rust
use autumn_web::config::AutumnConfig;

let config = AutumnConfig {
    server: my_server_config,
    ..AutumnConfig::default()
};
```

The new field is `pub cost: CostConfig`. Its default turns everything off, so an
app that does not set `[cost]` behaves as before. `CostConfig` is
`#[non_exhaustive]`: set its fields on a default value.

**Automation:** `manual` — a codemod cannot know which fields a struct literal
means to leave at their defaults.

### HTTP client: `retries(n)` no longer retries `POST` and `PATCH`

**Why:** `.retries(n)` also turned on retries for non-idempotent methods. A
request for more retries could then send a `POST` two times with no
`Idempotency-Key` (issue #3054).

**Before (`0.8`):**

```rust
// Retried the POST up to 2 times.
client.post(url).retries(2).send().await?;
```

**After (`0.9`):**

```rust
// Retries the POST up to 2 times. Sends an `Idempotency-Key` header
// (the same value on each attempt) if the request has none.
client.post(url).retries(2).retry_non_idempotent().send().await?;
```

If you build a `RetryPolicy`, `HttpClientConfig` or `JobConfig` with a
struct literal, add the new fields (`max_backoff`, `max_backoff_ms`, and on
`JobConfig` also `default_timeout_ms`), or end the literal with
`..Default::default()`.

**Automation:** `manual` — the fix adds a call only where you want `POST` or
`PATCH` retried. A codemod cannot know which calls are safe to repeat.

### Scheduler: the Postgres backend needs the `autumn_scheduler_ticks` table

**Why:** The advisory lock freed a tick when the leader finished, so a late
replica ran the tick again (issue #3052). A row in a table keeps the tick
claimed.

**Before (`{X.Y}`):** `scheduler.backend = "postgres"` needed no table. Each
tick held one pool connection while it ran.

```toml
[scheduler]
backend = "postgres"
```

**After (`{(X+1).0}`):** the same config. On first use the runtime creates
`autumn_scheduler_ticks`. If the app's database role cannot run
`CREATE TABLE`, apply the DDL before you deploy. Then grant `SELECT`,
`INSERT`, `DELETE` on the table and `USAGE` on its sequence:

```rust
// The DDL to apply:
let ddl = autumn_web::scheduler::PG_TICK_TABLE_DDL;
```

Three behaviour changes come with it:

- A row stays for the task's period plus `scheduler.lease_ttl_secs`. The
  period is the fixed delay, or the time to the next cron occurrence.
- **Rolling upgrade.** A new replica does not claim a tick while an old
  replica holds its advisory lock for that tick. But an old replica that
  reaches a tick after a new replica finished it runs the tick again, as old
  replicas did before. To prevent this, stop the old scheduler replicas
  before the new ones start (for example, deploy the `worker` role with a
  recreate strategy).
- A leader that crashes mid-tick does not free the tick. The next tick runs.
- `PostgresAdvisorySchedulerCoordinator` is a deprecated alias of
  `PostgresTickSchedulerCoordinator`.

**Automation:** `manual` - it is a database privilege change, and no code
rewrite applies.

### Metrics: `autumn_http_request_duration_seconds` is now a histogram

**Why:** You cannot add summaries from different replicas (issue #3064). A
histogram gives fleet p99 and SLO error ratios.

**Before (`{X.Y}`):** the family was a summary with quantile lines.

```promql
autumn_http_request_duration_seconds{version="canary",quantile="0.99"}
```

**After (`{(X+1).0}`):** the family is a histogram with `method`, `route`
and `status_class` labels. Compute a quantile from the buckets:

```promql
histogram_quantile(0.99,
  sum by (le, version) (rate(autumn_http_request_duration_seconds_bucket[5m])))
```

The old quantile lines stay, deprecated, under a new name:
`autumn_http_request_duration_quantiles_seconds`. For a quick fix, rename
the family in your queries, alerts and canary gates. A later release removes
the summary.

**Automation:** `manual` - the change is in PromQL queries and dashboards,
not in Rust code.

### Telemetry: `TelemetryConfig` and `OtlpTraceRuntime` have a new field

**Why:** The OTLP sampler takes a ratio now (issue #3064).

**Before (`{X.Y}`):** a struct literal listed every field.

```rust
let telemetry = TelemetryConfig { enabled: true, /* every field */ strict: false };
```

**After (`{(X+1).0}`):** add `sample_ratio`, or fill the rest from
`Default`.

```rust
let telemetry = TelemetryConfig { enabled: true, ..TelemetryConfig::default() };
```

`OtlpTraceRuntime` has a new `sample_ratio: SampleRatio` field too.

**Automation:** `manual` - a codemod cannot pick the ratio for you.

### Query budgets: an associated function handed the handle is reported (#2316)

**Why:** `Post::published(&mut db)` and `ReportBuilder::build(&mut db)` have
the same shape, so the analysis cannot tell a one-query finder from a helper
that loops. It counted both as 1 query, and a helper could hide an N+1.

This affects you only if a `#[query_budget(N)]` function gives a database or
repository handle to `Type::f(…)`. The build fails with "`f` is handed the
database handle". Put `#[query_cost(N)]` on the statement. An awaited
constructor of a handle type (`PgPostRepository::new(&mut db).await`) is
reported the same way.

**Before (`{X.Y}`):**

```rust
#[query_budget(1)]
async fn index(mut db: Db) -> AutumnResult<Markup> {
    let posts = Post::published(&mut db).await?;
    Ok(render(&posts))
}
```

**After (`{(X+1).0}`):**

```rust
#[query_budget(1)]
async fn index(mut db: Db) -> AutumnResult<Markup> {
    #[query_cost(1)]
    let posts = Post::published(&mut db).await?;
    Ok(render(&posts))
}
```

**Automation:** `manual` — the cost of each helper is a fact only its author
knows, so no codemod can write the `N` in `#[query_cost(N)]`.

### Query budgets: a method that borrows a container of handles is reported (#2316)

**Why:** Rust looks for a `self` method before a `&self` method, and for a
`&self` method before a `&mut self` method. So an application trait method
`len(self)` on `Vec<PgPostRepository>` runs in place of `Vec::len`, and it
can query. The analysis has no type information to rule that out.

This affects you only if a `#[query_budget(N)]` function calls a method that
borrows a `Vec`, `Option`, `Result`, map or set of a handle type
(`repos.len()`, `repos.push(repo)`, `repos.iter()`, `maybe.as_ref()`). The
build fails with "`len` is called on a container of database handles". Read
the container with an index, a pattern, a `for` loop or a method that takes
`self`, or put `#[query_cost(N)]` on the statement.

**Before (`{X.Y}`):**

```rust
#[query_budget(1)]
async fn first(repos: Vec<PgPostRepository>) -> AutumnResult<usize> {
    let n = repos.len();
    let Some(repo) = repos.first() else { return Ok(0) };
    Ok(n + repo.find_all().await?.len())
}
```

**After (`{(X+1).0}`):**

```rust
#[query_budget(1)]
async fn first(repos: Vec<PgPostRepository>) -> AutumnResult<usize> {
    #[query_cost(0)]
    let n: usize = repos.len();
    let [repo, ..] = &repos[..] else { return Ok(0) };
    Ok(n + repo.find_all().await?.len())
}
```

What a borrowing method gives may hold handles, so give its binding a type
made only of standard and primitive types (`let n: usize`). Without it, a
later use of `n` is reported too.

**Automation:** `manual` — the fix depends on what the call reads, and no
codemod can choose between an index, a pattern and `#[query_cost(N)]`.

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

---

## Plugin authors

Everything here is addressed to someone maintaining an `autumn-plugin-*` /
`autumn-*-plugin` crate, not to an application author. See
[`docs/plugins.md`](../plugins.md#the-plugin-api-contract) for the tiers.

- **Stable surface changed:** none.
- **Experimental surface changed:** none.
- **New stable surface:** none.
- **Declared range to move to:** each release, bump the literal in
  `Plugin::contract`'s `.autumn_web("…")` to the new series (or write it with
  `lockstep_range(env!("CARGO_PKG_VERSION"))` if the plugin releases in
  lockstep with the framework) and re-run
  `autumn plugin-check --plugin-name <your-plugin>`. A range that excludes the
  host makes `AppBuilder::plugin` panic at registration unless
  `AUTUMN_PLUGIN_CONTRACT=warn` demotes it. The examples in the `Plugin::contract`
  and `plugin_contract` rustdoc and the reference plugin's note name the series
  under development; copy the shape, not the literal.

## Compiler error cheat sheet

Paste the most common errors a user will hit and the fix. This is the
single most valuable section of the guide — keep it factual and short.

| Error message (truncated) | Where you see it | Fix |
|---------------------------|------------------|-----|
| `error[E0432]: unresolved import \`autumn_web::foo\`` | module reorganized | `use autumn_web::<new path>;` |
| `error[E0061]: this function takes 2 arguments but 1 was supplied` | `App::run` added a parameter | see [Breaking changes › {Area}] |
| `error[E0063]: missing field \`max_backoff\`` (or `max_backoff_ms`, `default_timeout_ms`) | a `RetryPolicy`, `HttpClientConfig` or `JobConfig` literal | add the field, or `..Default::default()` |
| `error[E0063]: missing field \`timeout\`` | a `JobInfo` literal | add `timeout: None` |
| `error[E0061]: this function takes 6 arguments but 5 arguments were supplied` | a direct call to `autumn_web::commentable::comment_thread` (or `add_comment`, `delete_comment`, `recompute_comment_count`) | add `None` as the last argument; see [Commentable](#commentable-the-runtime-helpers-take-soft_delete-optionbool) |

## Configuration changes

- `autumn.toml` keys that were renamed, removed, or have new defaults.
- New `AUTUMN_*` environment variables.
- Default profile changes.

If nothing changed, delete this section.

- New: `[http.client] max_backoff_ms` (default `20000`). The cap on the
  HTTP client's retry backoff.
- New: `[jobs] max_backoff_ms` and `AUTUMN_JOBS__MAX_BACKOFF_MS` (default
  `3600000`, 1 h). The cap on job retry backoff for every backend.
- New key `telemetry.sample_ratio` (`AUTUMN_TELEMETRY__SAMPLE_RATIO`),
  default `1.0`. See [Overload signals](../guide/observability/overload-signals.md).
- New: `[jobs] default_timeout_ms` and `AUTUMN_JOBS__DEFAULT_TIMEOUT_MS`
  (default `0`, no limit). The longest one run of a job without
  `#[job(timeout)]` may take (issue #3051).
- **Resilience (issue #3060):** new keys `slow_call_duration_threshold_ms`
  (default `60000`, `0` turns detection off), `slow_call_rate_threshold`
  (default `1.0`) and `cancelled_call_outcome` (default `"slow"`) under
  `[resilience.circuit_breaker.defaults]` and host overrides. The env
  variables are `AUTUMN_RESILIENCE__CIRCUIT_BREAKER__DEFAULTS__SLOW_CALL_DURATION_THRESHOLD_MS`,
  `..._SLOW_CALL_RATE_THRESHOLD` and `..._CANCELLED_CALL_OUTCOME`.

## Behavior changes

Changes that still compile but behave differently at runtime. Examples:

- Error responses adopted a new JSON shape.
- A default middleware is now ordered differently.
- A scheduled task now runs on a different worker.

- A trusted proxy can now set the request id. When
  `[security.trusted_proxies]` trusts the peer and the peer sends a UUID in
  `X-Request-Id`, the app keeps it. Before, the app always made a new id.
- `http_client` sends the current request's id as `x-request-id`. A header
  that you set on the request builder wins.
- A metric `method` label shows `_other` for a non-standard HTTP method. The
  `/actuator/metrics` JSON route keys change too: `PROPFIND /x` is now
  `_other /x`.
- To stop `http_client` from sending the request id to a host, call
  `.without_request_id()` on the request builder.

If nothing changed, delete this section.

- **Jobs: a hung handler keeps its claim (#3051).** Durable workers renew each
  claim while the job runs. Before, a hung handler lost its claim after the
  visibility timeout, and a second worker ran the job again. Now the claim
  stays until the process stops. Set `#[job(timeout = "...")]` or
  `jobs.default_timeout_ms` on a job that can hang.
- **Jobs: Redis claim deadlines use the Redis server clock (#3051).** During a
  rolling deploy, an old worker still compares deadlines to its own clock. An
  old worker whose clock runs ahead of Redis can requeue a live job. Keep
  worker clocks in sync (NTP) during the deploy.
- Retries use full jitter (issue #3054). The delay before retry `n` is a
  random value in `[0, min(cap, base * 2^n)]`. Before, the HTTP client and the
  `postgres`, `redis` and `sqlite` job backends used the exact value
  `base * 2^n`, and the `local` backend used `[base/2, base]`. A test that
  expects an exact retry time must allow the range.
- The HTTP client now obeys `Retry-After` on a `503`, as on a `429`. The wait
  is `backoff + min(hint, 5 s)`. Before, a `429` waited the full hint (up to
  `max_retry_after`, default 10 s) plus the backoff. A `429` with
  `Retry-After: 10` now retries after about 5 s. Against a strict rate
  limiter, raise `max_retries` or handle the `429` yourself.
- A `POST` or `PATCH` with `.retries(n)` and no `.retry_non_idempotent()`
  now makes one attempt. This compiles with no warning, so search your code
  for `.post(` and `.patch(` calls that use `.retries(`.
- **Resilience (issue #3060):** a circuit breaker opens when all calls in
  its window take 60 s or more. A call dropped at or after the slow-call
  threshold counts as slow. Before, it counted as nothing. To keep the old
  behaviour, set `slow_call_duration_threshold_ms = 0`.
- **Resilience (issue #3060):** a breaker keeps its counts in 10 time
  buckets. A call leaves the window after 9/10 to 10/10 of
  `sample_window_secs`. Before, it left after exactly `sample_window_secs`.
- **Commentable (#2284):** a model can have a `soft_delete` repository and a
  plain one. Through the plain repository, the `{Model}Comments` helpers now
  accept a soft-deleted parent. Before, they returned `404`. Through the
  `soft_delete` repository and through the router, a soft-deleted parent is
  still `404`.

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
5. Commentable (#2284): soft-delete a parent row. Call `comment_thread` on it
   through a plain repository of the model. Make sure that it returns the
   thread, not `404`.

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
