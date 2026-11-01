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

### Features: 16 modules are now optional

**Why:** These modules made `autumn-web` slower to build for apps that do not
use them (issue #2795, ADR 0027). Each one now has a feature. All of them are
in `default`, so a default build is the same as before.

**Who is affected:** An app that sets `default-features = false` and uses one
of these modules. The compiler reports `unresolved import` or `cannot find
module`. An app that keeps the default features needs no change.

| Feature | Module or API |
|---------|---------------|
| `alerts` | `alerts`, `with_alert_channel`, `[alerts]` |
| `custom-domains` | `custom_domain`, tenant custom domains |
| `data-retention` | `data_retention`, `autumn db retention` (needs `gdpr`) |
| `experiments` | `experiments` (needs `feature-flags`) |
| `feature-flags` | `feature_flags`, `with_flag_store` |
| `gdpr` | `gdpr`, data capsules |
| `graph` | `graph`, `/actuator/graph` |
| `idempotency` | `idempotency`, `SubmitTokenLayer` |
| `job-tracking` | the `/_autumn/jobs/{token}` status route |
| `ledger` | `ledger` (needs `version-history`) |
| `money` | `money` |
| `nested-form` | `nested_form` |
| `slo` | `slo`, `[[slo]]` |
| `upgrade` | `upgrade`, `with_live_state`, `SIGUSR2` hand-over |
| `version-history` | `version_history` |
| `wizard` | `wizard` |

The `db` feature turns on `ledger`, `money`, `data-retention`,
`idempotency` and `graph`. The `acme` feature turns on `custom-domains`.

**Before (`{X.Y}`):**

```toml
autumn-web = { version = "{X.Y.Z}", default-features = false, features = ["maud"] }
```

**After:**

```toml
autumn-web = { version = "{X.Z.0}", default-features = false, features = ["maud", "feature-flags"] }
```

Without `idempotency`, the one-time submit token does nothing, because it
stores its tokens in the idempotency store. In production the app logs a
warning. The `SubmitToken` and `SubmitFormField` extractors still compile.

Without `alerts` or `slo`, the `[alerts]` and `[[slo]]` sections still parse.
They do nothing, and the app logs a warning. Without `job-tracking`, the
status route is not mounted. Without `upgrade`, the app ignores `SIGUSR2`.

**Automation:** `manual` — the right feature depends on which modules the app
uses, so no codemod can choose it.

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

### Billing: `EventClaim::Duplicate` is now `Applied` and `InFlight`

**Why:** a redelivery that met an unfinished claim was acknowledged. If the
first attempt then failed, the provider had no reason to retry, and the event
was lost (issue #3097).

**Automation:** `manual` — a store must decide what `InFlight` means for it, so
no codemod applies.

**You are affected only if you implement `BillingStore` or match on
`EventClaim` or `ReconcileOutcome`.** `EventClaim` is now `#[non_exhaustive]`.
`ReconcileOutcome` has a new variant. It was already `#[non_exhaustive]`, so a
wildcard arm covers it.

**Before (`{X.Y}`):**

```rust
match store.claim_event(id, kind, now, stale_after).await? {
    EventClaim::Claimed => { /* apply */ }
    EventClaim::Duplicate => { /* acknowledge */ }
}
```

**After (`{(X+1).0}`):**

```rust
match store.claim_event(id, kind, now, stale_after).await? {
    EventClaim::Claimed => { /* apply */ }
    EventClaim::Applied => { /* acknowledge */ }
    EventClaim::InFlight => { /* do not acknowledge: ask for a retry */ }
    _ => {}
}
```

A store returns `Applied` for a finished row, and `InFlight` for an
unfinished row that is not stale. `reconcile::apply` returns
`ReconcileOutcome::InFlight` for `InFlight`. The webhook route answers `503`,
so the provider redelivers.

### Widgets: alert, badge and wizard classes have the `autumn-` prefix

**Why:** every other widget class has the `autumn-` prefix since 0.8.0.
Three families did not. A generic class such as `alert` or `badge` can
collide with app CSS (issue #3396).

**You are affected only if your own CSS, JS or tests target the old class
names.** The markup is the same apart from the class names.

**Before (`{X.Y}`):**

```css
.alert-error { border-width: 2px; }
.badge--success { text-transform: uppercase; }
.wizard-step--current .wizard-step__label { color: red; }
```

**After (`{(X+1).0}`):**

```css
.autumn-alert--error { border-width: 2px; }
.autumn-badge--success { text-transform: uppercase; }
.autumn-wizard-step--current .autumn-wizard-step__label { color: red; }
```

Full map:

| Old | New |
| --- | --- |
| `alert` | `autumn-alert` |
| `alert-info`, `alert-success`, `alert-warning`, `alert-error` | `autumn-alert--info`, `autumn-alert--success`, `autumn-alert--warning`, `autumn-alert--error` |
| `alert__icon`, `alert__content`, `alert__title`, `alert__body`, `alert__errors`, `alert__dismiss`, `alert__dismiss-toggle` | `autumn-alert__icon`, `autumn-alert__content`, `autumn-alert__title`, `autumn-alert__body`, `autumn-alert__errors`, `autumn-alert__dismiss`, `autumn-alert__dismiss-toggle` |
| `badge` | `autumn-badge` |
| `badge--neutral`, `badge--info`, `badge--success`, `badge--warning`, `badge--danger` | `autumn-badge--neutral`, `autumn-badge--info`, `autumn-badge--success`, `autumn-badge--warning`, `autumn-badge--danger` |
| `wizard-progress`, `wizard-progress__summary` | `autumn-wizard-progress`, `autumn-wizard-progress__summary` |
| `wizard-step`, `wizard-step--completed`, `wizard-step--current`, `wizard-step--upcoming` | `autumn-wizard-step`, `autumn-wizard-step--completed`, `autumn-wizard-step--current`, `autumn-wizard-step--upcoming` |
| `wizard-step__number`, `wizard-step__label` | `autumn-wizard-step__number`, `autumn-wizard-step__label` |

A test that calls `.assert_selector(".wizard-progress")` must use
`.autumn-wizard-progress`. `autumn generate wizard` writes the new name.

**Automation:** `manual` — the selectors are in the app's own stylesheets,
scripts and tests. A codemod cannot find them reliably.

### Widgets: `NavMenu` is a popover menu

**Why:** `NavMenu` now uses the same primitive as `dropdown_menu`: a native
`popover` opened by `command="toggle-popover"` and `commandfor` (issue
#3402). The browser handles open, close, light dismiss, Escape and the top
layer.

**You are affected only if your own CSS, JS or tests target the old markup.**

- With JavaScript off, a menu starts closed. Before, it was an open list.
  The trigger opens it with no JavaScript in a browser with invoker
  commands (Baseline Dec 2025).
- The trigger has no `data-nav-menu-toggle`. Use `aria-haspopup="menu"`.
- The menu has no `hidden` attribute. It is closed when it is not
  `:popover-open`. A rule on `.autumn-nav__menu[hidden]` does nothing.
- The server now sends `role="menu"`, `role="menuitem"` and
  `aria-haspopup="menu"`. The runtime sets `aria-expanded`.
- The menu is in the top layer, so `z-index` on `.autumn-nav__menu` does
  nothing.

**Before (`{X.Y}`):**

```css
.autumn-nav__menu[hidden] { display: none; }
```

```rust
page.assert_selector("[data-nav-menu-toggle]");
```

**After (`{(X+1).0}`):**

```css
.autumn-nav__menu:not(:popover-open) { display: none; }
```

```rust
page.assert_selector("[aria-haspopup=menu]");
```

**Automation:** `manual` — the selectors are in the app's own stylesheets
and tests.

### Jobs: `JobTrackingStore` has a required `update_record` method

**Why:** Each built-in store had the same code for `mark_running`,
`set_progress`, `complete` and `fail` (issue #3208). These methods now have
default bodies that call `update_record`. A store implements only
`update_record`. The trait applies the status rules and the `0..=100` progress clamp.
`get` also has a default body, which calls `get_with_token`.

**Who is affected:** Only code that implements `JobTrackingStore`. The
compiler reports error `E0046` (`update_record` is missing). Code that only
calls a store does not change.

**Before (`{X.Y}`):**

```rust
use futures::future::BoxFuture; // = Pin<Box<dyn Future<Output = T> + Send + 'a>>

impl JobTrackingStore for MyStore {
    fn mark_running<'a>(&'a self, key: &'a str) -> BoxFuture<'a, AutumnResult<()>> {
        Box::pin(async move { self.write(key, |r| r.status = TrackedJobStatus::Running).await })
    }
    // set_progress, complete, fail, get: the same shape
    // create, get_with_token, reset_for_retry
}
```

**After (`{(X+1).0}`):**

```rust
use futures::future::BoxFuture; // = Pin<Box<dyn Future<Output = T> + Send + 'a>>

impl JobTrackingStore for MyStore {
    fn update_record<'a>(
        &'a self,
        key: &'a str,
        f: &'a (dyn Fn(&mut TrackedJobRecord) + Send + Sync),
    ) -> BoxFuture<'a, AutumnResult<()>> {
        Box::pin(async move { self.write(key, f).await })
    }
    // create, get_with_token, reset_for_retry
}
```

`update_record` must do nothing when `key` is unknown or expired. It can call
`f` more than one time, for example after a failed compare-and-swap. Give `f`
a fresh copy of the record each time. Set `updated_at` after `f`. Change the
`RetryToken` and refresh the TTL on each write. Remove your `mark_running`, `set_progress`,
`complete`, `fail` and `get`. If you keep them, they override the defaults.

**Automation:** `manual` — `update_record` needs the store's own storage code.
A codemod cannot write it.

### Middleware: ingress service futures are not boxed

**Why:** a boxed future is one heap allocation on each request. The ingress
services now return named futures (issue #2830).

**You are affected only if your code names the future type of one of these
services, or polls the future without pinning it:**

- `LogContextService`
- `SessionService`
- `MethodOverrideService`
- `IdempotencyReplayService`
- the services of `TrustedProxiesLayer` and `SubmitTokenLayer`

The futures of `LogContextService` and `SessionService` can be `!Unpin`.

**Before (`{X.Y}`):**

```rust
let mut fut: Pin<Box<dyn Future<Output = _> + Send>> = service.call(request);
let response = poll_fn(|cx| fut.as_mut().poll(cx)).await;
```

**After (`{(X+1).0}`):**

```rust
let fut = service.call(request);
let response = fut.await;
// Or, to keep a boxed future:
let mut fut = Box::pin(service.call(request));
```

To name the type, use `<S as Service<Request>>::Future`.

**Automation:** `manual` — the code to change is in the app's own middleware.
A codemod cannot find it reliably.

### Database: `DatabaseFleetConfig` has three new fields

**Why:** a deleted fleet database's replica now goes with it (issue #3235).
`replica_retention_after_delete` keeps it for an undo window.
`restore_check_sample` sets the restore check. `replication_workers` sets the
number of threads that ship fleet databases.

**You are affected only if your code builds a `DatabaseFleetConfig` with a
struct literal** (tests and tools often do). TOML and `AUTUMN_*` config need
no change: all three keys have defaults.

**Before (`{X.Y}`):**

```rust
DatabaseFleetConfig {
    mode: FleetMode::Tenant,
    root: "/var/lib/app/fleet".to_owned(),
    path: None,
    max_open: 256,
    pool_size: 2,
    create_on_demand: None,
    idle_close_secs: 300,
    restore_missing: false,
    ownership: None,
}
```

**After (`{(X+1).0}`):**

```rust
DatabaseFleetConfig {
    mode: FleetMode::Tenant,
    root: "/var/lib/app/fleet".to_owned(),
    path: None,
    max_open: 256,
    pool_size: 2,
    create_on_demand: None,
    idle_close_secs: 300,
    restore_missing: false,
    ownership: None,
    replica_retention_after_delete: std::time::Duration::ZERO,
    restore_check_sample: 1,
    replication_workers: 4,
}
```

**Automation:** `manual` — a struct literal needs the values the app wants.
A codemod cannot choose them.

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
- **New stable surface:** `plugin_conformance::is_sensitive_path`,
  `normalize_path_for_collision` and `is_plugin_asset_path` are public.
  The plugin-check CLI uses the same rules. Nothing else changed.
- **New stable surface (widget packs, #3400):** `widget_pack` (the widget
  contract and `WidgetPack`), `AppBuilder::widget_pack`,
  `AppBuilder::with_story_gallery` and `form::FieldRenderer` (with
  `FieldControl::Custom`, `FieldContext`, `CustomControl` and
  `custom_input_with`). `plugin_conformance::widgets` checks a pack. See
  [`docs/guide/widget-packs.md`](../guide/widget-packs.md).
- **`with_story_gallery` adds (#3400):** a second call now merges into the
  gallery. Before, it replaced the gallery. A plugin gallery starts from
  `StoryGallery::new()`. If it starts from `StoryGallery::builtin()` and the
  app does too, startup panics on a duplicate story slug.
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

- **Widgets follow the OS dark mode (#3396).** The widget stylesheet sets
  `color-scheme: light dark`, and each color token has a dark value. On a
  dark OS, fields, cards, alerts, modals and toasts become dark. A page whose
  own colors are light only (for example Tailwind `bg-gray-50` on `<body>`)
  then shows dark widgets on a light page. To keep the light scheme, set it
  on `<html>`:

  ```rust
  html lang="en" data-theme="light" { /* … */ }
  ```

  To support both schemes, use the tokens (`var(--bg)`, `var(--text)`) for
  the page colors, and see `docs/guide/widget-styling.md#dark-mode`.
- **`with_story_gallery` adds (#3400).** Each call now adds its stories to
  the gallery. Before, a second call replaced the first gallery. If your app
  calls it two times with the same stories, startup now panics on a
  duplicate story slug. Call it one time with all your stories.
- **`autumn-widgets.js` starts on `DOMContentLoaded` (#3400).** Before, a
  deferred `autumn-widgets.js` started immediately. Now all deferred scripts
  run first. Thus a widget pack script gets the page-load `autumn:init`
  event. An async or injected copy still starts immediately.

- **Toasts use the top layer (#3399).** With `autumn-widgets.js`,
  `toast_region` opens as a popover in the top layer. `--z-toast` and
  `z-index` do not apply there. The runtime removes a toast from the DOM
  when its fade ends. Before, the toast stayed hidden in the DOM.
  `.autumn-toast` is now `display: flex` and holds a close button. An
  `Error` toast has `role="alert"` on `.autumn-toast__message`, not on
  `.autumn-toast`. A page with neither the widget CSS nor the JS no longer
  shows the region: the browser hides a closed popover. Link
  `/static/css/autumn-widgets.css`.
- **Money: each account row stores its balance (#3339).** The framework
  migration `20261029183512_add_money_account_balance` adds the
  `_autumn_money_accounts.balance_minor` column and a trigger. Run
  `autumn migrate` before you deploy the new binary. The new binary reads the
  column. Without the migration, each ledger call fails with
  `LedgerError::Database`. The migration does not read the postings, so it is
  fast. Old and new binaries can post at the same time, because the trigger
  keeps the balance for both. The migration holds a short exclusive lock on
  `_autumn_money_accounts` and `_autumn_money_postings`. To roll back, deploy
  the old binary first, then run `down.sql`. Do not write `balance_minor`
  yourself. `ledger::balance_drift` reports a difference. After a data-only
  restore, set `balance_minor` to `NULL`. See
  `docs/guide/money.md#the-stored-balance`.

- **Radio choice ids: a name or value with a special character has a new id
  (#3454).** Before, two groups could give the same choice id. That breaks the
  `<label for>` link. Now letters, digits and `_` do not change. Each other
  byte (`-`, `.`, a space, non-ASCII) is written `--HH` (upper-case hex). The
  name `plan-type` gives `plan--2Dtype-x`. In group `speed`, the value
  `next-day` gives `speed-next--2Dday`. An id of only letters, digits and `_`
  does not change. An empty name after an `id_prefix` is now written `--`. The
  second of two equal values now ends in `--n1`. It ended in `-<index>`
  before. Update a CSS rule, a test or a `getElementById` call that uses an
  old id.

- **A fleet delete removes the replica (#3235).** With `[replication]` on,
  `DatabaseFleet::delete` and `autumn fleet delete` now delete the
  database's objects in object storage too. Before, they stayed. To keep
  them for a time, set `database.fleet.replica_retention_after_delete`. A
  delete now needs the control database: when it cannot write the tombstone
  row, it fails with `503` and removes nothing. `autumn fleet delete` also
  needs the replica destination and its credentials, and `autumn fleet
  restore` needs the control database.
- **A create on demand of a deleted key answers `410` during the retention
  (#3235).** With `replica_retention_after_delete` set, `create_on_demand`
  does not make the key again until the retention ends. `provision` does.
- **`restore_missing` does not restore a deleted database (#3235).** Before,
  the next open of a deleted key restored it from the leftover replica.
  `fleet.restore` and `autumn fleet restore` still bring it back.
- **The fleet runs a restore check (#3235).** Every `[replication]
  verify_interval_secs` (default 6 hours), each worker-role process restores
  one random database into a scratch file. The first check runs one interval
  after start. Set `database.fleet.restore_check_sample = 0` to turn it off.

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
