# Migrating to the next Autumn release (rolling draft)

> **Rolling draft.** This is the in-flight guide for the changes currently
> under `## [Unreleased]` in [`CHANGELOG.md`](../../CHANGELOG.md). Every PR
> that lands a breaking change appends a section here and links this file from
> its changelog entry. At release time the file is renamed to
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

### Audit: `AuditEvent` gains a `metadata` field

**Why:** A retention sweep has to record three facts — which dataset, what
cutoff, and how many rows it removed (issue #1605) — and `AuditEvent` had
nowhere to put them. `metadata` is a flat `BTreeMap<String, String>` rather
than arbitrary JSON so `AuditEvent` keeps `Eq` and a deterministic,
key-ordered serialization, which is the shape SIEM ingestion expects. It is
`#[serde(default)]` and skipped when empty, so **archives written before this
release still deserialize and existing archive lines are unchanged.**

Only code that constructs or destructures `AuditEvent` *by struct literal* has
to change. `AuditEvent::new(...)` is unaffected, and so is every read of an
existing field.

**Before (`{X.Y}`):**

```rust
use autumn_web::audit::{AuditEvent, AuditStatus};

let event = AuditEvent {
    timestamp: chrono::Utc::now(),
    actor_id: "admin-1".into(),
    action: "user.role.update".into(),
    target_resource_id: "user-99".into(),
    ip_address: None,
    status: AuditStatus::Success,
};
```

**After (`{X.Z}`):**

```rust
use autumn_web::audit::{AuditEvent, AuditStatus};

// Preferred: the constructor, which fills `metadata` with an empty map.
let event = AuditEvent::new(
    "admin-1",
    "user.role.update",
    "user-99",
    None,
    AuditStatus::Success,
);

// …and, where the extra detail is useful:
let event = event.with_metadata("reason", "promoted by support ticket 4412");
```

A struct literal still works if you add `metadata: Default::default()`, but
prefer the constructor — it is the form that survives the next field.

**Automation:** `manual` — this needs a value for a new field (or a switch to
the constructor), which no mechanical rewrite can choose safely.

Also additive, and requiring no change: `AuditSink` gains a **provided**
`purge_before(cutoff, dry_run)` method that defaults to reporting
"unsupported". Existing sinks keep compiling untouched. Override it if your
sink stores audit events somewhere that can be pruned in place and you want
`retention.audit_archives` to reach it — see
[Data Retention for Framework-Owned Data](../guide/data-retention.md).

### SSG: `ManifestEntry` / `StaticManifest` are `#[non_exhaustive]`, and generated pages carry their declared `Content-Type`

**Why:** The static-first serve path used to reverse-engineer each cached page's
`Content-Type` at request time from the route slug and the served file name.
Because every non-root route is stored as `<route>/index.html`, both clues lie,
and the heuristic needed three consecutive corrections during review of #1819.
`autumn build` now records the type each handler declares into
`dist/manifest.json` and the middleware serves it verbatim (issue #1832). The
manifest types were sealed in the same release so a future field cannot break
callers again.

**Before (`{X.Y}`):**

```rust
use autumn_web::static_gen::{ManifestEntry, StaticManifest};

let entry = ManifestEntry {
    file: "about/index.html".to_owned(),
    revalidate: Some(3600),
};
let manifest = StaticManifest {
    generated_at: timestamp(),
    autumn_version: "{X.Y.Z}".to_owned(),
    routes,
};
```

**After (`{(X+1).0}`):**

```rust
use autumn_web::static_gen::{ManifestEntry, StaticManifest};

let entry = ManifestEntry::new("about/index.html")
    .with_revalidate(Some(3600))
    .with_content_type(Some("text/html; charset=utf-8".to_owned()));
// `new` stamps `generated_at` (Unix-epoch seconds) and `autumn_version` for
// you; chain `.with_generated_at(fixed)` to pin a reproducible timestamp.
let manifest = StaticManifest::new(routes);
```

Exhaustive *destructuring* is sealed too, not just literals — `let
ManifestEntry { file, revalidate } = entry;` now fails with E0638. Add `..` to
the pattern (`let ManifestEntry { file, revalidate, .. } = entry;`) or read the
fields directly.

The on-disk JSON is unaffected in both directions: an existing `dist/` loads
with `content_type` absent and keeps its previous derived types, and a manifest
written by this release still carries every key a pre-#1832 runtime requires, so
a rollback or a rolling deploy sharing one `dist/` volume keeps serving
statically.

**Automation:** `manual` — the rewrite is not a rename or an import move. A
struct literal has to become a constructor call plus a variable number of
`with_*` setters chosen from which fields the literal actually set, and
`StaticManifest::new` *drops* two of the fields the old literal supplied
(stamping them itself), so no purely syntactic rewrite is correct. Both types
are niche (only an app that reads or writes `dist/manifest.json` itself touches
them), so `cargo check` pointing at each E0639/E0063 is enough.

#### Behaviour change: extensionless `#[static_get]` routes returning `String`

Nothing is recorded unless the handler *deliberately* declared a type, so
routes that name their own extension are unaffected: `/theme.css` returning a
`String` still serves `text/css`, because axum's `text/plain; charset=utf-8` is
a default from the return type rather than a statement about the page.

That check is by value, because axum's inferred default and a hand-written
declaration of the same type produce byte-identical responses. Only axum's two
exact spellings are treated as inferred, so if you deliberately want
`text/plain` or `application/octet-stream` on a route whose extension is in
Autumn's asset table, declare it distinctly — bare `text/plain`, or
`application/octet-stream` with a parameter — and it is recorded. Prefer
`Content-Disposition: attachment` for downloads. Extensions outside the asset
table (`.pdf`, `.zip`) always keep the declared type.

An **extensionless** route is the one visible change. `#[static_get("/about")]
async fn about() -> String { html }` has no extension to fall back on, so it is
now served as the `text/plain; charset=utf-8` axum declares, instead of the
`text/html` the old heuristic assumed — the same thing that route already served
on the dynamic path (in dev, or on a manifest miss). Return `Markup` or
`Html<String>` and the page is `text/html` on both paths:

```rust
#[static_get("/about")]
async fn about() -> Markup { html! { h1 { "About" } } }   // text/html
```

For a non-HTML route, declare the type explicitly — this is now the whole
contract, and it is what makes types the old heuristic could never produce
(`application/rss+xml` from `/feed`) work at all:

```rust
#[static_get("/feed")]
async fn feed() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "application/rss+xml")], build_feed())
}
```

An existing `dist/` records nothing and keeps its previous derived behaviour
until the next `autumn build`.

One operational note: ISR does not rewrite the manifest, so a regeneration whose
handler declares a *different* type than was recorded is refused (logged at
`error`, with the previous page still served) rather than writing bytes the
recorded header would mislabel. Re-run `autumn build` after changing a static
route's `Content-Type`.

### Failure capsules: `capsule::execute` takes `ReplayFixtures`, and the capsule format is version 3

**Why:** Capsules now record every framework effect a failing run produced —
outbound HTTP, job enqueues, cache reads and writes, mail sends, the resolved
tenant, and the random bytes it drew (issue #1634) — and replay serves all of
them from the capsule. A replay is only deterministic if the clock, the entropy
source and the effect tape all come from the *same* capsule, so `execute` takes
one value that bundles them instead of a loose clock.

Two consequences beyond the signature. The document's `format_version` bumps
`2 → 3`, so **capsules recorded by an older Autumn are refused** rather than
replayed with every new seam empty — a reader that tolerated them would report a
verdict on an application shape production never ran. And an outbound HTTP call
during replay is now *served from the capsule* instead of failing closed; a call
the capsule never recorded is still refused, and is reported as a divergence.

**Before:**

```rust
let clock = ReplayClock::new(capsule.clock.clone(), fallback);
let outcome = capsule::execute(router, &capsule, divergences, Some(&clock)).await;
```

**After:**

```rust
let fixtures = ReplayFixtures::from_capsule(&capsule);
// The router is now built *through* the fixtures, so the clock and the
// entropy source it serves come from the same capsule the verdict judges.
let router = TestApp::new()
    .routes(routes![charge])
    .with_clock(fixtures.clock())
    .with_entropy(fixtures.entropy())
    .build()
    .into_router();
let outcome = capsule::execute(router, &capsule, divergences, &fixtures).await;
```

Two smaller source breaks travel with it, both from added fields on
non-`#[non_exhaustive]` types, so only struct-literal construction and
wildcard-less `match`es are affected:

* `ClientError` gains `ReplayedRequestFailure(String)` — a recorded outbound
  transport failure, reproduced as a failure rather than downgraded to a
  status. A `match` on `ClientError` without a `_` arm needs it.
* `Capsule` gains `effects` and `job`, and `ReplayOutcome` gains
  `effect_divergences`. Build capsules with
  `capsule::schema::test_support::capsule(...)` (behind `test-support`) rather
  than a struct literal, or add `..Default::default()`-style fields explicitly.

One behaviour change worth knowing even though it does not break compilation:
during a replay an outbound HTTP call is now **served from the capsule**
instead of failing closed. A call the capsule never recorded is still refused,
and is reported as an effect divergence rather than silently dialling. The
recording answers a call only when its method, URL, caller-set headers *and*
body match — the same endpoint with a different payload is a divergence — and
a mail send is compared against its recorded body as well as its recipients
and subject — and against its sender when the replayed run chose one, since a
message that sets no `from` inherits `[mail] from` at send time and a replay
boots without mail configuration.

Three situations now produce a refusal or an incomplete capsule where an
earlier build would have graded the run. A **job capsule whose payload was
masked** by `[log] filter_parameters` is refused: a handler is handed its
payload verbatim, so it would parse the `[FILTERED]` placeholder rather than
the value production ran on. A run that **enqueued inside its own
transaction** (`enqueue_on_conn`) marks its capsule incomplete, because that
enqueue is also a job-row INSERT on the database tape which replay can serve
but never issue. And an **effect whose future was cancelled** before it
finished — a losing `tokio::select!` branch, a timeout — does the same rather
than persist an outcome the run never had. Each case previously produced a
verdict; each now says why it cannot.

Two API changes come with the sharper failure reproduction. `ClientError` gains
`ReplayedRequestFailure` and `MailError` gains `ReplayedFailure`, both of which
print their recorded text and nothing else; a `match` on either enum without a
`_` arm needs the new variant. Recorded failures are otherwise rebuilt as the
variant production produced, so code branching on
`ClientError::CircuitBreakerOpen` or `MailError::AllRecipientsSuppressed`
behaves during replay as it did live.

A **panicking job** now leaves a capsule on whatever attempt it panicked on.
All three backends dead-letter a panic immediately regardless of remaining
attempts, so a job configured with `max_attempts = 25` that panics on its
first attempt never reaches a "final" one — and under the previous gate never
produced a capsule at all. Ordinary failures are unchanged: still captured on
the final attempt only.

Capsules already on disk are not migrated: replay them with the version that
wrote them, or re-record the failure. A committed regression corpus is
re-recorded and re-converted the same way — `autumn capsule verify` reports
every stale capsule as `UNREADABLE` and exits non-zero, deliberately, so a
corpus cannot quietly stop testing anything. See
[Failure Capsules › Compatibility across Autumn versions](../guide/failure-capsules.md).

**Automation:** `manual` — the new argument is a value the caller has to
construct from the capsule, and there is no textual rewrite that can invent it;
`autumn upgrade` ships no codemod for this. Direct callers of `capsule::execute`
are limited to code that drives replay itself, which is rare outside the
framework.

---

### Config: `AutumnConfig` gains a `replication` field

**Why:** Continuous SQLite replication (issue #1628) needs its own configuration
section, and `AutumnConfig` has all-public fields with no `#[non_exhaustive]`, so
adding one is a breaking change for anyone constructing the struct literally.
Almost nobody does — `AutumnConfig::default()` and `..Default::default()` are the
documented ways to build one, and both keep compiling.

**Before (`{X.Y}`):**

```rust
use autumn_web::config::AutumnConfig;

let config = AutumnConfig {
    server: my_server_config,
    database: my_database_config,
    // …every other field spelled out…
};
```

**After (`{(X+1).0}`):**

```rust
use autumn_web::config::AutumnConfig;

let config = AutumnConfig {
    server: my_server_config,
    database: my_database_config,
    ..AutumnConfig::default()
};
```

The new field is `pub replication: Option<Box<ReplicationConfig>>`, `None` by
default, so `..Default::default()` needs no other change. Nothing about an app
that does not configure `[replication]` behaves differently.

**Automation:** `manual` — a struct-literal expression can only be rewritten by
knowing which fields the caller meant to leave defaulted, which a codemod cannot
infer; the fix is the one-line `..AutumnConfig::default()` above.

### Config: `JobConfig` gains a `sqlite` field, and `SchedulerBackend` gains a `Sqlite` variant

**Why:** Durable jobs and a single-host scheduler on SQLite (issue #1907) need
their own configuration. `JobConfig` has all-public fields and `SchedulerBackend`
is not `#[non_exhaustive]`, so both additions are breaking for anyone who
constructs the struct literally or matches the enum exhaustively.

**Before (`{X.Y}`):**

```rust
use autumn_web::config::{JobConfig, SchedulerBackend};

let jobs = JobConfig {
    backend: "postgres".to_owned(),
    workers: 4,
    // …every other field spelled out…
};

let label = match config.scheduler.backend {
    SchedulerBackend::InProcess => "single process",
    SchedulerBackend::Postgres => "fleet",
};
```

**After (`{(X+1).0}`):**

```rust
use autumn_web::config::{JobConfig, SchedulerBackend};

let jobs = JobConfig {
    backend: "postgres".to_owned(),
    workers: 4,
    ..JobConfig::default()
};

let label = match config.scheduler.backend {
    SchedulerBackend::InProcess => "single process",
    SchedulerBackend::Postgres => "fleet",
    SchedulerBackend::Sqlite => "processes on one host",
    _ => "unknown",
};
```

The new field is `pub sqlite: JobSqliteConfig`, whose `Default` is a 30-second
visibility timeout and a 250ms poll interval, so `..Default::default()` needs no
other change. `SchedulerBackend::Sqlite` reports `is_fleet_distributed() == true`
— it coordinates several processes, on one host.

Nothing changes for an app that keeps `jobs.backend` and `scheduler.backend` as
they are.

**Automation:** `manual` — a struct literal can only be rewritten by knowing
which fields the caller meant to default, and a match arm needs a decision about
what the new variant means for that call site.

### Capacity contracts: three metadata structs gain fields

Deploys can now carry a proven capacity contract (`autumn calibrate` →
`capacity.lock` → `[server] capacity_contract`; see
[the guide](../guide/capacity-contracts.md)). Nothing about existing behaviour
changes — an app that configures no contract sheds exactly as before — but the
feature adds fields to three public, non-`#[non_exhaustive]` structs, so
**struct-literal construction** of them no longer compiles:

* `openapi::ApiDoc` gains `pools: &'static [&'static str]` — the pool tags a
  handler's declared extractors prove it holds.
* `route_listing::RouteInfo` gains `resource_shape: ResourceShape` and
  `pools: Vec<String>`.
* `config::ServerConfig` gains `capacity_contract: Option<String>`.

All three derive `Default`, so the fix is to end the literal with
`..Default::default()` — which is what every construction site inside the
workspace already does, and what the route macros emit. Field access, pattern
matching with a `..` rest, and deserialization of an older routes dump are all
unaffected: the two `RouteInfo` fields are `#[serde(default)]`.

**Automation:** `manual` — `autumn upgrade` ships no codemod for this. The edit
is mechanical (end the literal with `..Default::default()`), but a rewrite
cannot tell an exhaustive struct literal that *wants* to name every field from
one that simply predates these three, and appending a rest pattern to the wrong
literal would silently paper over a genuinely missing value on a later field
addition. Direct struct-literal construction of all three types is rare outside
the framework: `ApiDoc` and `RouteInfo` are macro-emitted, and `ServerConfig` is
normally deserialized from `autumn.toml`.

### ACME: `AcmeRenewalTask` gains `dns` and `recovery`, `AcmeConfig` gains `dns`

Wildcard certificates over the DNS-01 challenge
([the guide](../guide/tls.md#wildcard-certificates-via-dns-01-servertlsacmedns))
add two fields to `acme::renewal::AcmeRenewalTask` and one to
`config::AcmeConfig`. Everything here is behind the off-by-default `acme`
feature, and an app that configures no `[server.tls.acme.dns]` section behaves
exactly as before — HTTP-01, unchanged.

* `AcmeRenewalTask::dns: Option<acme::renewal::DnsChallenge>` — the DNS-01
  wiring. `None` keeps issuance on the HTTP-01 path.
* `AcmeRenewalTask::recovery: Option<acme::renewal::RecoveryFn>` — invoked after
  an issuance that succeeded following a recorded failure, so the app can clear
  the operator alert its reporter raised.
* `AcmeConfig::dns: Option<config::AcmeDnsConfig>` — the deserialized
  `[server.tls.acme.dns]` section.

Only **struct-literal construction** is affected, which in practice means test
harnesses: the framework builds `AcmeRenewalTask` itself, and `AcmeConfig` is
deserialized from `autumn.toml`. Neither type derives `Default`, so the fix is
to name the fields.

**Before (`{X.Y}`):**

```rust
let task = AcmeRenewalTask {
    resolver,
    provider,
    store,
    cert_id,
    tokens,
    status,
    config,
    serving_stored_cert: false,
    leadership_degraded: false,
    renew_window_misconfigured: AtomicBool::new(false),
};
```

**After (`{X.Z}`):**

```rust
let task = AcmeRenewalTask {
    // …unchanged fields…
    renew_window_misconfigured: AtomicBool::new(false),
    dns: None,      // Some(DnsChallenge { .. }) to issue over DNS-01
    recovery: None, // Some(callback) to clear an operator alert on recovery
};
```

The new **output** types in `acme::dns` — the parsed DNS answer, its records,
the propagation-timeout detail, the credential, and the HTTP request/response —
are `#[non_exhaustive]` from the start, so fields can be added to those without
a break. `AcmeConfig`, `AcmeDnsConfig`, `AcmeRenewalTask` and `DnsChallenge`
deliberately are not: callers construct them by struct literal and they have no constructor, so
sealing them would make them unbuildable outside this crate. That is the same
trade-off `config::ServerConfig` makes.

**Automation:** `manual` — `autumn upgrade` ships no codemod. The edit is two
lines in a test harness, and a rewrite cannot tell an `AcmeRenewalTask` literal
that means "HTTP-01" from one whose author intended to configure DNS-01;
defaulting to `None` silently would be right in the first case and wrong in the
second, which is exactly the choice a human should make.

## Plugin authors

This release **adds** plugin-facing surface and removes none, so no plugin that
compiles today stops compiling. Two things do change for you: `autumn
plugin-check` gains a check your plugin has to satisfy (see the end of this
section), and `ConformanceConfig` is now `#[non_exhaustive]`.

See [`docs/plugins.md`](../plugins.md#the-plugin-api-contract) for the full
contract and [`STABILITY.md`](../../STABILITY.md#the-plugin-api-surface-issue-1601)
for the policy.

- **Stable surface changed:** one, and only for a construction style nothing
  in this repository used. **Breaking:** `plugin_conformance::ConformanceConfig`
  gains a `contract` field and is now `#[non_exhaustive]`, so a struct literal
  (`ConformanceConfig { plugin_name, expected_prefix, .. }`) no longer compiles.
  Build it the documented way instead — the fluent constructors are unchanged:

  ```rust,ignore
  let config = ConformanceConfig::new("autumn-plugin-mine")
      .prefix("/mine")
      .sensitive_route("/mine", "Role: admin required");
  ```

  `#[non_exhaustive]` lands with the field on purpose: it is what lets a later
  release add a check's configuration without doing this to you twice. See the
  [migration guide](next.md).

  Nothing else was removed, renamed, or re-signatured. The surface that already
  existed is now *declared* stable in
  `autumn_web::plugin_contract::PLUGIN_SURFACES` and compiled on every commit by
  the pinned `autumn-plugin-reference` crate.
- **Experimental surface changed:** none. `AppBuilder::with_edge_kv` and
  `autumn_edge::host` are now labelled experimental, matching what
  `STABILITY.md` already said about the edge capsule lane (issue #1790).
- **New stable surface:**
  - `Plugin::contract` — declare the `autumn-web` range your plugin supports.
    Defaults to `None`, so implementing it is optional.
  - `autumn_web::plugin_contract` — `PluginContract`, `PLUGIN_SURFACES`,
    `SurfaceTier`, `evaluate`, `lockstep_range`, `lockstep_contract`, and the
    `PLUGIN_CONTRACT_MARKER` dump protocol.
  - `AppBuilder::plugin_contracts()` — the contracts declared by the plugins
    mounted on a builder.
  - `ConformanceConfig::contract(...)` and
    `plugin_conformance::check_experimental_surface(...)` — the
    `experimental-surface` check, runnable from your own test suite.
  - `autumn_web::db::Pool` and `autumn_web::reexports::diesel_migrations` — so a
    plugin implementing `DatabasePoolProvider` or shipping migrations through
    `AppBuilder::plugin_migrations` can name what those seams need without
    taking its own `diesel-async` / `diesel-migrations` dependency. (Both were
    found by writing the reference plugin: the seams were declared plugin-facing
    and were not reachable from stable API.)
- **Declared range to move to:** `autumn-web {X.Y}` — add
  `.autumn_web("{X.Y}")` to your `Plugin::contract` and re-run
  `autumn plugin-check --plugin-name <your-plugin>`. A plugin that releases in
  lockstep with the framework can write
  `.autumn_web(lockstep_range(env!("CARGO_PKG_VERSION")))` instead and never
  touch the literal again — or write the whole `Plugin::contract` body in one
  call with the new `lockstep_contract(env!("CARGO_PKG_NAME"),
  env!("CARGO_PKG_VERSION"))`, which every first-party plugin in this
  repository now does (previously each wrote out the three-call construction
  by hand).

**Breaking for the `plugin-check` command, and only for it.** Two checks join
the report. `plugin-contract` **fails** when the plugin under check declares no
usable `autumn-web` range; `experimental-surface` reports what the plugin
declares and fails only on a name that cannot be resolved against the registry.

So: your plugin still *compiles and runs* unchanged whether or not it declares
a contract — that part is genuinely additive. But if your CI runs
`autumn plugin-check --plugin-name <your-plugin>`, it goes red until you
implement `Plugin::contract`. That is deliberate: this is the author-facing
gate, and "you have not said which framework versions you support" is exactly
what it exists to say. Implementing the four-line `contract()` above clears it.

Both checks **skip** against a host binary built on an `autumn-web` that
predates the contract dump, so an older host app does not turn them red — but
`--deny-experimental` fails closed in that case rather than silently passing,
because a flag that forbids something must not quietly become a no-op.

`autumn generate plugin` now scaffolds `Plugin::contract` and a conformance
test that passes it, so a freshly generated plugin is green out of the box.

## Compiler error cheat sheet

Paste the most common errors a user will hit and the fix. This is the
single most valuable section of the guide — keep it factual and short.

| Error message (truncated) | Where you see it | Fix |
|---------------------------|------------------|-----|
| `error[E0432]: unresolved import \`autumn_web::foo\`` | module reorganized | `use autumn_web::bar;` |
| `error[E0061]: this function takes 2 arguments but 1 was supplied` | `App::run` added a parameter | see [Breaking changes › {Area}] |

## Configuration changes

**New `[server.tls.acme.dns]` section** (additive; absent means HTTP-01, exactly
as before). It names a DNS provider and the *credentials-store key* holding that
provider's API credential — never the credential itself. The section is
`deny_unknown_fields`, so an `api_token = "..."` written into `autumn.toml` is a
startup error naming the key rather than a plaintext secret nobody notices:

```toml
[server.tls.acme]
domains = ["myapp.com", "*.myapp.com"]   # a wildcard is now accepted…
contact_email = "ops@myapp.com"

[server.tls.acme.dns]                    # …when this section is present
provider = "cloudflare"                  # cloudflare | route53 | exec
```

A wildcard entry in `[server.tls.acme] domains` is rejected at startup **unless**
this section is configured, because no CA validates a wildcard identifier over
HTTP-01.

**New environment variables**, which override the encrypted credentials store
field for field:

| Variable | Field |
|---|---|
| `AUTUMN_ACME_DNS_API_TOKEN` | `api_token` (Cloudflare) |
| `AUTUMN_ACME_DNS_ACCESS_KEY_ID` | `access_key_id` (Route 53) |
| `AUTUMN_ACME_DNS_SECRET_ACCESS_KEY` | `secret_access_key` (Route 53) |
| `AUTUMN_ACME_DNS_SESSION_TOKEN` | `session_token` (Route 53) |
| `AUTUMN_ACME_DNS_HOSTED_ZONE_ID` | `hosted_zone_id` (Route 53) |
| `AUTUMN_ACME_DNS_REGION` | `region` (Route 53) |

### New: `[replication]` (continuous SQLite replication)

Optional and off by default; an app that does not add the section is unaffected.
Every key has an `AUTUMN_REPLICATION__*` environment override, and credentials
are named by environment variable rather than inlined — the same posture as
`[backup.offsite]`. See
[SQLite in production → Durability](../guide/sqlite-in-production.md#durability-continuous-replication-and-point-in-time-restore).

When `[replication] enabled = true`, pooled SQLite connections are created with
`PRAGMA wal_autocheckpoint = 0` so the replicator is the only component that
checkpoints. That is a deliberate behaviour change for replicating apps and is
described in the guide; apps without the section keep SQLite's default
auto-checkpointing.

### New: `[jobs.sqlite]` (durable SQLite job queue)

Read only when `jobs.backend = "sqlite"`; every other backend ignores it. Two
keys, both with `AUTUMN_JOBS__SQLITE__*` environment overrides:
`visibility_timeout_ms` (default 30 000) bounds how long a claim a crashed
worker left behind stays unreclaimed, and `poll_interval_ms` (default 250) sets
how fast an idle worker sees work another process enqueued. See
[SQLite in production → Durable jobs without Redis](../guide/sqlite-in-production.md#durable-jobs-without-redis).

`scheduler.backend = "sqlite"` needs no new section: it reuses
`scheduler.lease_ttl_secs` and `scheduler.key_prefix`.

## Behavior changes

### CI: `autumn upgrade` adds a blocking dependency audit — add `deny.toml` with it

`.github/workflows/ci.yml` is framework-owned, so `autumn upgrade --apply`
reconciles it and your project picks up the new dependency-advisory gate
(issue #1600): it runs `cargo deny check advisories` on every push and fails
the build on a known RustSec advisory.

That audit reads a `deny.toml` at your project root, which `autumn new` writes
for new projects and the upgrade deliberately does **not** create — its waiver
list is yours, and a file you are asked to edit must never come back as an
upgrade conflict. Add it in the same commit as the upgrade:

Generate a throwaway project with the release you are upgrading to, **using the
same flags you originally scaffolded with**, and copy its policy in. The flags
matter: `--bundled-pg` apps pull the embedded-Postgres build stack, so their
policy carries a second waiver (`RUSTSEC-2024-0384`, `instant`) that other
flavors deliberately do not ship. A donor generated without your flags installs
a policy that fails your first audit.

```bash
cd /tmp
autumn new policy-donor          # add your original flags here, e.g. --bundled-pg
cd -
cp /tmp/policy-donor/deny.toml ./deny.toml
rm -rf /tmp/policy-donor
git add deny.toml
```

Without it the audit step stops before auditing and tells you exactly this —
it does not fall back to an unwaived default policy and fail on an advisory
Autumn has already triaged (`rsa`/RUSTSEC-2023-0071, which reaches every
Autumn app through `jsonwebtoken` and has no patched release).

Then read [the advisory gate](../guide/supply-chain.md#part-3a--the-advisory-gate-known-vulnerable-dependencies)
for how to read a failure and how to waive an advisory. Never disable the step
to get green: an edited `ci.yml` becomes a conflict on every later upgrade.

Other changes that still compile but behave differently at runtime. Examples:

- Error responses adopted a new JSON shape.
- A default middleware is now ordered differently.
- A scheduled task now runs on a different worker.

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
