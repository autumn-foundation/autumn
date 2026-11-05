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
- **MSRV delta:** `1.88.0` → `1.97.0` (set by the maintainers; dependencies
  need 1.95; see [Toolchain](#toolchain-rust-1970-or-newer))
- **Carried dependency majors:** `reqwest 0.12 → 0.13`,
  `webauthn-rs 0.5 → 0.6`, `p256 0.13 → 0.14`, `serde-saphyr 1 → 2`

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

### Features: four widget families are now optional

**Why:** An app that used `card` and `nav_bar` compiled every widget, and
every page got all the widget CSS and JavaScript (issue #3408, ADR 0034). The
widgets are now in families. The core widgets need only `maud`. Four families
have a feature. All four are in `default`, so a default build is the same as
before. Every `autumn_web::widgets::*` path stays the same.

**Who is affected:** An app that sets `default-features = false` and uses a
widget of one of these families. The compiler reports `unresolved import`. An
app that keeps the default features needs no change.

| Feature | Widgets |
|---------|---------|
| `widgets-search` | `active_search*`, `autocomplete_*`, `ActiveSearchConfig`, `AutocompleteConfig`, `SearchMethod` |
| `widgets-social` | `reaction_controls`, `comment_thread`, `comment_dom_id`, `feed_page`, `infinite_feed` and their config types; the `#[commentable]` router |
| `widgets-workflow` | `transition_controls*`, `TransitionLabels`, `bulk_*`, `BulkActionsConfig` |
| `widgets-charts` | `sparkline*`, `bar_chart*`, `line_chart*`, `ChartConfig` |

Each family feature turns on `maud`.

**Before (`{X.Y}`):**

```toml
autumn-web = { version = "{X.Y.Z}", default-features = false, features = ["maud", "htmx"] }
```

**After:**

```toml
autumn-web = { version = "{X.Z.0}", default-features = false, features = ["maud", "htmx", "widgets-search"] }
```

Without `widgets-search`, `/static/js/autumn-widgets.js` has no autocomplete
runtime. Without a family, `/static/css/autumn-widgets.css` has no rules for
its classes. The gallery at `/_stories` shows only the stories of the enabled
families.

A build without `maud` loses `SearchMethod`, `ActiveSearchConfig`,
`AutocompleteConfig` and `comment_dom_id`. Before, they compiled without
`maud`. Now each one needs its family feature, which turns on `maud`.
`localized_path`, `DEFAULT_TOAST_REGION_ID` and `DEFAULT_TOAST_DURATION`
still need no feature.

**Automation:** `manual` — the right family depends on which widgets the app
uses, so no codemod can choose it.

### Features: `jwt` and `push` are now optional

**Why:** Every app compiled `jsonwebtoken` and `p256`, whether or not it
verified a JWT or sent a Web Push. `jsonwebtoken` brings in `rsa` 0.9
(RUSTSEC-2023-0071, no fixed release) and `crypto-bigint` 0.5
(RUSTSEC-2026-0387, fixed only in 0.7.6, which elliptic-curve 0.13 cannot
take). An app that used neither still had to waive both in its `cargo deny`
policy. Each crate is now behind its own feature. Both are in `default`, so a
default build is the same as before.

**Who is affected:** An app that sets `default-features = false` and uses JWT
tenancy, OIDC or Web Push. An app that keeps the default features needs no
change.

| Feature | Owns | Covers |
|---------|------|--------|
| `jwt` | `jsonwebtoken` | `tenancy.source = "jwt"`, and the OIDC `id_token` check. `oauth2` turns on `jwt`. |
| `push` | `p256` | `autumn_web::push` (`WebPush`, `PushMessage`, `[push]`), `AppBuilder::with_web_push`, `AppBuilder::with_push_subscription_store`, and the matching `TestApp` methods |

**Before (`{X.Y}`):**

```toml
autumn-web = { version = "{X.Y.Z}", default-features = false, features = ["maud", "htmx"] }
```

**After, for an app that uses JWT tenancy and Web Push:**

```toml
autumn-web = { version = "{X.Z.0}", default-features = false, features = ["maud", "htmx", "jwt", "push"] }
```

A build without a feature refuses the config that needs it, before it binds
a port, and names the fix:

- `[push]`, or any `AUTUMN_PUSH__*` variable, without `push`: the boot stops
  with exit code 1. The `[push]` table still parses, so a strict config file
  stays valid.
- `tenancy.enabled = true` with `tenancy.source = "jwt"`, without `jwt`: the
  boot stops with exit code 1. A router built without `run` fails closed: tenant
  resolution returns a 500, and never reads a tenant from an unverified token.

An app that leaves out both features (and `oauth2`) compiles neither
`jsonwebtoken`, `rsa` nor `crypto-bigint` 0.5, and can delete the
`RUSTSEC-2023-0071` and `RUSTSEC-2026-0387` waivers from its `deny.toml`. A
passkey app (`webauthn`) still reaches `rsa` through `webauthn-rs`.

**Automation:** `manual` — whether an app verifies a JWT or sends a push is
not visible from its `Cargo.toml`.

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

### Security: the CSRF extractors cannot reject

**Why:** a required `CsrfToken` answered `500` when CSRF was off (the dev
default), so one handler did not work in every profile.

**You are affected only if your code names the rejection type** of
`CsrfToken`, `CsrfFormField` or `CsrfTokenHeader`, for example in a custom
extractor. It is now `std::convert::Infallible`.

**Before (`{X.Y}`):**

```rust
let token = CsrfToken::from_request_parts(parts, state).await.ok();
```

**After (`{(X+1).0}`):**

```rust
// `None` when CSRF is off, as `.ok()` gave before.
let token = parts.extensions.get::<CsrfToken>().cloned();
```

**Automation:** `manual` — the replacement depends on what the code wants
when CSRF is off.

### Repositories: a SQLite build defines `Sqlite<Trait>`

**Why:** the generated repository was named only `Pg<Trait>`, also on
SQLite.

**You are affected only if a SQLite app defines its own type named
`Sqlite<Trait>`** (for example `SqliteNoteRepository`) in the module of a
`#[repository] trait NoteRepository`. Rename your type, or use the new
alias.

**Automation:** `manual` — a codemod cannot choose the new name.

### Database: `DatabaseFleetConfig` has four new fields

**Why:** a deleted fleet database's replica now goes with it (issue #3235).
`replica_retention_after_delete` keeps it for an undo window.
`restore_check_sample` sets the restore check. `replication_workers` sets the
number of threads that ship fleet databases. `min_residency` sets how long
the `max_open` sweep keeps a database that was just used.

**You are affected only if your code builds a `DatabaseFleetConfig` with a
struct literal** (tests and tools often do). TOML and `AUTUMN_*` config need
no change: all four keys have defaults.

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
    min_residency: std::time::Duration::from_secs(2),
}
```

**Automation:** `manual` — a struct literal needs the values the app wants.
A codemod cannot choose them.

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
with `..Default::default()`.

### Widgets: `AutocompleteConfig` has five new fields

**Why:** `autocomplete_input` has a tags mode (issue #3410). `multiple`
turns it on. `selected` holds the first chips. `remove_label` is the
`aria-label` of each remove button. `added_message` and `removed_message`
are the status texts.

**You are affected only if your code builds an `AutocompleteConfig` with
a struct literal.** Code that calls `AutocompleteConfig::new` needs no
change.

**Before (`{X.Y}`):**

```rust
AutocompleteConfig {
    action: "/tags/autocomplete",
    value_name: "tag_id",
    // … the other fields …
    free_text: false,
    initial_value: None,
}
```

**After (`{(X+1).0}`):**

```rust
AutocompleteConfig {
    action: "/tags/autocomplete",
    value_name: "tag_id",
    // … the other fields …
    free_text: false,
    initial_value: None,
    multiple: false,
    selected: &[],
    remove_label: "Remove {label}",
    added_message: "{label} added",
    removed_message: "{label} removed",
}
```

**Automation:** `manual`. Or use `AutocompleteConfig::new` and the
builder methods. Then you need no change.

### Config: an unknown `AUTUMN_ROLE` stops the boot

**Why:** a typo such as `AUTUMN_ROLE=workers` booted the app as `combined`
(issue #3541). A worker pod then served user routes and ran the cron
scheduler. The warning went to stderr before logging started, so it was easy
to miss. A bad `role` in `autumn.toml` already stopped the boot.

**You are affected only if a deployment sets `AUTUMN_ROLE` to a value that
is not a role.** Run `autumn doctor` with the deployment env. The
`process_role_backend` row fails on such a value.

**Before (`{X.Y}`):**

```text
Warning: AUTUMN_ROLE="workers" is not valid (expected combined, web, or worker), ignoring
```

The app booted as `combined`.

**After (`{(X+1).0}`):**

```text
configuration error: AUTUMN_ROLE: unknown process role "workers" (accepted: combined, all, web_and_worker, server_and_worker, web, server, http, worker, jobs, worker_only), and no [roles.workers] is declared (no roles are declared)
```

The app does not boot. Set a valid value, or remove the variable to get
`combined`. An empty `AUTUMN_ROLE=` is still unset.

The `autumn` CLI commands that load the config also read `AUTUMN_ROLE`. Some
of them do not show the load error. For example, `autumn migrate` can then
report "No database URL found". If a CLI command fails in this way, examine
`AUTUMN_ROLE` in your shell.

**Automation:** `manual` — the fix is in the deployment env, not in code.

### Config: `state.role()` returns `&Role`; `config.role` is a `RoleName`

**Why:** a process can now run a named `[roles.<name>]` preset (issue #3543).
`ProcessRole` is a closed enum and cannot hold a name. The new `Role` holds
the name, the built-in base and the capability set.

**You are affected only if your code compares `state.role()` with a
`ProcessRole`, stores it as a `ProcessRole`, or sets `config.role`.** A call
such as `state.role().serves_http()` or `state.role().runs_workers()` compiles
as before.

**Before (`{X.Y}`):**

```rust
let role: ProcessRole = state.role();
if state.role() == ProcessRole::Worker { /* … */ }
config.role = ProcessRole::Web;
```

**After (`{(X+1).0}`):**

```rust
let role: ProcessRole = state.role().base();
if state.role().base() == ProcessRole::Worker { /* … */ }
config.role = ProcessRole::Web.into();
```

`Role == ProcessRole` is `true` only for that built-in role. A preset over
`worker` is not equal to `ProcessRole::Worker`. Compare `base()` to include
the presets. `state.role().name()` gives the name.

A top-level `[roles]` table now belongs to the framework. If your
`autumn.toml` uses `[roles]` for app data, move that data to another key. The
load fails on it otherwise.

A custom config loader that skips `validate()` and sets an undeclared
`AUTUMN_ROLE` name no longer boots with a warning. The boot stops.

**Automation:** `manual` — the right fix depends on whether the code means
"this built-in role" or "every tier on this base".

### Routing: `Route` and `AutumnConfig` have new fields

**Why:** route groups and per-role mounting (issue #3544) add `Route::group`
and `AutumnConfig::route_groups`. These structs have public fields and are
not `#[non_exhaustive]`, so an old struct literal does not compile. The
route macros set `group` for you.

**You are affected only if your code builds a `Route { .. }` literal, builds
an `AutumnConfig { .. }` literal with no `..Default::default()`, or uses a
top-level `[route_groups]` table for app data.**

**Before (`{X.Y}`):**

```rust,ignore
let route = autumn_web::Route {
    // …
    criticality: autumn_web::Criticality::Default,
    seo: Default::default(),
};
```

**After (`{(X+1).0}`):**

```rust,ignore
let route = autumn_web::Route {
    // …
    criticality: autumn_web::Criticality::Default,
    seo: Default::default(),
    group: None,
};
```

`group: None` puts the route in the group of the code that registers it:
the `routes_in` group, else the plugin group, else `app`. A role without `mount` serves every group, so
nothing else changes.

`RouteInfo` has a new field `group`, and `graph::manifest::Completeness` has
a new field `mounted_elsewhere`. Both derive `Default`. Use
`..Default::default()` in a literal.

A top-level `[route_groups]` table now belongs to the framework. If your
`autumn.toml` uses it for app data, move that data to another key.

**Automation:** `manual` — a codemod cannot know which group a hand-built
route belongs to.

### Toolchain: Rust 1.97.0 or newer

**Why:** The maintainers set the MSRV to 1.97.0. The newest dependency
releases need at least 1.95 (`sysinfo` 0.40); the AWS SDK needs 1.94.1.
1.97.0 was released on 2026-07-09. That is less than the 6 months that
[STABILITY.md](../../STABILITY.md#minimum-supported-rust-version-msrv-policy)
asks for a minor-release MSRV bump, so this change is breaking.

**Who is affected:** A project that builds with Rust older than 1.97.0. Cargo
stops with `rustc 1.88.0 is not supported by the following package:
autumn-web@… requires rustc 1.97.0`.

**Before (`{X.Y}`):**

```toml
# rust-toolchain.toml
[toolchain]
channel = "1.88.0"
```

**After (`{(X+1).0}`):**

```toml
# rust-toolchain.toml
[toolchain]
channel = "1.97.0"
```

Set `rust-version = "1.97.0"` in `Cargo.toml`. Set `msrv = "1.97.0"` in
`clippy.toml`. Change the toolchain pin in your CI. Change the `Dockerfile`
base image to `rust:1.97.0-bookworm`.

**Automation:** `manual` — `autumn upgrade` offers the new
`rust-toolchain.toml` and `clippy.toml`, but your CI and `Dockerfile` pins
are yours.

### HTTP client: `reqwest` 0.13

**Why:** `autumn-web` and `autumn-media-plugin` now use `reqwest` 0.13. An app
that passes `reqwest` types to Autumn must use 0.13 too.

**Who is affected:**

- An app that uses one of these public items with its own `reqwest` types:
  - `autumn_web::http::ClientError::Request` (a `reqwest::Error`, and its
    `From<reqwest::Error>` impl). This is in the default `http-client`
    feature.
  - `autumn_web::shadow::HttpShadowTransport::new`, which returns a
    `reqwest::Error`.
  - The `oauth2` API: `HttpClient::new`, `HttpRequestBuilder::send`,
    `HttpInterceptor` and `HttpInterceptorFuture`.
  - `autumn_media_plugin::recording_available`, which takes a
    `reqwest::Client`.

  With `reqwest` 0.12 in your `Cargo.toml`, the compiler reports
  `mismatched types` and notes that there are multiple different versions of
  crate `reqwest` in the dependency graph.
- An app that builds its own `reqwest` client with no TLS feature. Before,
  `autumn-web` turned on `rustls-tls` for every `reqwest` 0.12 user. Now
  `autumn-web` turns on `rustls-no-provider` and gives its own clients a
  `ring` config. Your `ClientBuilder::build()` (or `Client::new()`) panics at
  runtime with "No rustls crypto provider is configured". Turn on a TLS
  feature (for example `rustls`), or install a rustls `CryptoProvider` first.
  (With `managed-pg`, `postgresql_embedded` installs `ring` as the process
  provider before its first download. Install your own provider earlier if
  you need a different one.)

**Before (`{X.Y}`):**

```toml
reqwest = { version = "0.12", default-features = false, features = ["json"] }
```

**After (`{(X+1).0}`):**

```toml
reqwest = { version = "0.13", default-features = false, features = ["json", "rustls"] }
```

`reqwest` 0.13 also moves `.form()` and `.query()` behind the `form` and
`query` features. See the upstream notes below.

**Automation:** `manual` — the features you need depend on how your code
uses `reqwest`.

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
- **New stable surface (no-route plugins):** `ConformanceConfig::no_routes()`
  and the `ConformanceConfig::no_routes` field. A plugin that mounts no routes
  now passes `run_conformance` in `cargo test`, like
  `autumn plugin-check --plugin-name <your-plugin> --no-routes`. Pass the
  contract from `AppBuilder::plugin_contracts()` with it, so a route under the
  `registered_as` name is found; without one, every plugin route fails. Code
  that does not call it sees no change. `ConformanceConfig` is `#[non_exhaustive]`, so the new field breaks
  no struct literal.
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

- **The graph counts routes that another role serves (#3544).** When an app
  declares a `[roles.<name>]` preset over `web` or `combined`, the graph of
  any other role (a worker, say) lists the routes that preset mounts under
  `completeness.mounted_elsewhere`, not under `unmounted_routes`. A committed
  graph of such a role changes once. Regenerate it with
  `autumn graph show --manifest <path>`.
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

- **The dev profile sets `session.secure = false`.** The dev server serves
  plain http on localhost, and most clients do not return a `Secure` cookie
  over http. Session sign-in then failed under `cargo run`. Other profiles
  keep `true`. To keep `true` in dev, set `[session] secure = true`.

- **A required `CsrfToken` works with CSRF off.** Before, a handler that took
  `CsrfToken`, `CsrfFormField` or `CsrfTokenHeader` answered `500` when
  `security.csrf.enabled = false` (the dev default). Now `CsrfToken` is empty
  and the names are the defaults. Nothing checks them. To know whether CSRF
  is on, take `Option<CsrfToken>`.

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

- **`reqwest` 0.13** ([changelog](https://github.com/seanmonstar/reqwest/blob/master/CHANGELOG.md)).
  See [HTTP client](#http-client-reqwest-013). `autumn-media-plugin` uses the
  `rustls` feature. Its clients now use `aws-lc-rs` and the operating-system
  root store, not `webpki-roots`. An image with no CA bundle cannot reach an
  https MediaMTX origin. Install `ca-certificates`.
- **`webauthn-rs` 0.6** ([repository](https://github.com/kanidm/webauthn-rs)).
  It uses pure-Rust crypto, so a new `--passkeys` app needs no OpenSSL. A
  passkey app generated before this release pins `webauthn-rs` 0.5, which
  still needs OpenSSL. To remove OpenSSL, change the requirement to `0.6`.
  Then change `cred_id.as_ref()` to `cred_id.as_slice()` in `encode_cred_id`
  (`CredentialID` is now `Vec<u8>`). Stored passkeys keep the same JSON
  format. Test one passkey login after the change.
- **`p256` 0.14** ([RustCrypto](https://github.com/RustCrypto/elliptic-curves)).
  Not in the public API. Its `crypto-bigint` 0.7 fixes RUSTSEC-2026-0387 on
  the Web Push path. The `jsonwebtoken` ES256/ES384 path still has it.
- **`serde-saphyr` 2.** `openapi.yaml` can change by a few bytes: a string
  with an inline `#` is no longer quoted. The document parses to the same
  values.

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
