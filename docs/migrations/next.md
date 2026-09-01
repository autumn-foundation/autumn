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

## Compiler error cheat sheet

Paste the most common errors a user will hit and the fix. This is the
single most valuable section of the guide — keep it factual and short.

| Error message (truncated) | Where you see it | Fix |
|---------------------------|------------------|-----|
| `error[E0432]: unresolved import \`autumn_web::foo\`` | module reorganized | `use autumn_web::bar;` |
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
