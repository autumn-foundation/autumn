# autumn-web API Reference (published 0.5.0 + trunk-dev)

Use this file as a quick map for public names, features, dependency versions,
and config keys. Verify against current source when exact code matters.

Version identity: crates.io serves **0.5.0** (the latest published release);
the `trunk-dev` workspace is versioned **0.6.0 (unpublished)**. Entries marked
**(unreleased)** exist on trunk-dev but NOT in the published 0.5.0 crates.

## Published crates

| Crate | Directory | Notes |
|---|---|---|
| `autumn-macros` | `autumn-macros/` | Proc macros; publish first |
| `autumn-web` | `autumn/` | Main framework crate; import path `autumn_web` |
| `autumn-cli` | `autumn-cli/` | Binary crate; binary name `autumn` |
| `autumn-admin-plugin` | `autumn-admin-plugin/` | First-party admin UI plugin |
| `autumn-storage-s3` | `autumn-storage-s3/` | S3-compatible `BlobStore` plugin |
| `autumn-cache-redis` | `autumn-cache-redis/` | Redis cache plugin |
| `autumn-search` | `autumn-search/` | Keyword + vector search plugin |

All publishable crates share the `[workspace.package]` version and release
together (`0.5.0` published; `0.6.0` on trunk-dev, unpublished).

## Top-level exports

### Functions

- `autumn_web::app() -> AppBuilder`

### Common types

- `AppState`
- `AutumnError`, `AutumnResult<T>`
- `Db`
- `Page<T>`, `PageRequest`, `CursorPage<T>`, `CursorRequest`
- `Valid<T>`, `Validated<T>`, `ValidateExt`
- `Redirect`
- `PathExt`
- `Markup`, `PreEscaped`, `html!`
- `Json`, `Path`, `Form`, `Query`, `State`
- `HTMX_JS_PATH`, `HTMX_CSRF_JS_PATH`, `HTMX_VERSION`

### Feature-gated top-level types

- `Mail`, `MailAttachment`, `Mailer`, `MailConfig`, `MailTransport`,
  `MailDeliveryQueue`, `MailDeliveryQueueHandle`, `Transport`, `SmtpConfig`,
  `TlsMode` (`mail`) — `MailPreview` is available via `autumn_web::mail::MailPreview`
  (not re-exported at the crate root)
- `DbApiTokenStore`, `API_TOKEN_MIGRATIONS`, repository hooks (`db`)
- `Multipart` (`multipart`)
- `Flash`, `FlashLevel`, `FlashMessage` (`flash`)
- `Broadcast`, `Channels`, `ChannelsBackend`, `LocalChannelsBackend`,
  `ChannelMessage`, `ChannelStats` (`ws`) — in tests, opt in with
  `TestApp::record_broadcasts()` and assert with `TestClient::broadcasts()` /
  `broadcasts_on(topic)` / `assert_broadcast(topic, predicate)` /
  `assert_broadcast_count(topic, n)` / `assert_no_broadcasts(topic)`
  (`RecordedBroadcast` exposes `.topic()` / `.payload()`)
- Resumable SSE **(unreleased — trunk-dev, issue #1356)**:
  `sse::stream_resumable(&state, topic, last_event_id: Option<u64>)` — automatic
  monotonic per-topic event ids + `Last-Event-ID` replay from a bounded
  per-topic ring buffer, with a `gap` sentinel (`event: gap`,
  `data: {"gap":true}`) on buffer overflow; `sse::last_event_id(&headers)` and
  the `sse::LastEventId(Option<u64>)` extractor read the inbound header;
  `Channels::resume(topic, last_event_id) -> ResumeHandle` (fields
  `subscriber`, `replay: Vec<SequencedMessage { id, message }>`, `gap`,
  `next_live_id`, `resumable`); `ChannelsBackend::resume` has a live-only
  default (Redis) overridden by `LocalChannelsBackend`;
  `LocalChannelsBackend::with_replay_capacity(capacity, replay_capacity)`.
  Retention is `channels.replay_buffer` (default `256`). The existing id-less
  `sse::stream` / `sse::stream_authorized` / `sse::from_subscriber` are
  unchanged.
- `TestClient` auth helpers: it carries a cookie jar that persists each
  response's `Set-Cookie` and replays it on later requests, so a real
  `POST /login` → `GET /dashboard` flow needs no manual header threading.
  `client.acting_as(user_id).await` (alias `login_as`) mints an authenticated
  session directly — writing the configured `auth.session_key` (default
  `user_id`) — so a `#[secured]` / `Auth` route returns its real success status
  without hitting the login endpoint; it sets identity only, so roles/scopes
  still run. `client.log_out()` clears the session cookie so secured routes
  reject again. Requires `TestApp::build()` with the default in-memory session
  backend; panics for `from_router` clients.
- `TestClient` job recorder: on by default for every `TestApp::build()` client
  (no `with_job_interceptor` opt-in), it captures every enqueue — across
  `enqueue`, `enqueue_after_commit`, and `enqueue_in_tx` — as a `RecordedJob`
  (`.name()` / `.payload()`). Read them with `client.enqueued_jobs()` and assert
  with `assert_job_enqueued(name)` / `assert_job_enqueued_with(name, payload)` /
  `assert_no_jobs_enqueued()`. `client.perform_enqueued_jobs().await` drains the
  queue and dispatches each captured job through its registered handler,
  returning a `PerformedJobs` report (`.assert_all_succeeded()`, `.failures()`,
  `.outcomes()`) that surfaces per-job handler errors — including payloads that
  fail the real deserialization round-trip — rather than swallowing them. The
  recorder is per-`TestApp` and composes ahead of any `with_job_interceptor`;
  `enqueued_jobs` / `perform_enqueued_jobs` panic for `from_router` clients.
- `Locale`, `t!` (`i18n`)
- OAuth2/OIDC config, provider presets, callback helpers, and identity values
  (`oauth2`)

## Proc macros

| Macro | Purpose |
|---|---|
| `#[get]`, `#[post]`, `#[put]`, `#[patch]`, `#[delete]` | HTTP route handlers; optional args `name`, `api_version`, `sunset_opt_out`, `timeout_ms`, `timeout = "off"`, and `seo(...)` |
| `routes![...]` | Collect route handlers |
| `#[autumn_web::main]` | Tokio runtime + Autumn profile bootstrap |
| `#[static_get]`, `static_routes![...]` | Static pre-render routes for `autumn build`; also accepts `params`, `revalidate`, and `seo(...)` |
| `#[ws]` | WebSocket route handler (`ws`) |
| `#[model]` | Diesel model derives (`db`) |
| `#[repository]` | CRUD repository and generated API (`db`); `mcp` / `mcp = "read"` expose the generated routes as MCP tools |
| `#[service]` | Service implementation scaffolding (`db`) |
| `#[secured]` | Session auth and role guard |
| `#[public]` | Marks a route handler as deliberately unauthenticated for the `autumn routes audit` coverage manifest — mirrors `#[secured]`, classifying the route `public` vs `gated`/`framework`/`unclassified` (**unreleased** — trunk-dev, #1604) |
| `#[authorize]` | Record-level policy guard |
| `#[api_doc]` | Route OpenAPI metadata |
| `#[oauth2_callback]` | OAuth2/OIDC callback route |
| `#[cached]` | Memoize function results |
| `#[scheduled]`, `tasks![...]` | Recurring scheduled tasks |
| `#[job]`, `jobs![...]` | Request-triggered background jobs |
| `#[task]`, `one_off_tasks![...]` | Operator tasks invoked by CLI |
| `paths![...]` | Typed route path helper module |
| `#[mailer]`, `#[mailer_preview]`, `mail_previews![...]` | Mail helpers (`mail`) |
| `t!(...)` | Compile-time checked translation lookup (`i18n`) |
| `#[feature_flag]` | Feature-flag definition |
| `#[inbound_mail]` | Inbound mail handler |
| `#[step_up]` | Step-up authentication guard |
| `#[throttle]` | Per-route rate limit — inline (`limit`/`per`/`key`) or named (`#[throttle("login")]`) (**unreleased**) |
| `#[event]`, `#[listener]`, `listeners![...]` | Typed domain event bus (**unreleased**) — publish via the `Events` extractor, register with `.listeners(...)` |

Route macros accept a `seo(...)` argument declaring per-page meta tag defaults
(**unreleased** — trunk-dev, #1182):
`#[get("/about", seo(title = "About", description = "…"))]`. Keys mirror the
`SeoMeta` builder — `title`, `description`, `canonical`, `og_title`,
`og_description`, `og_image`, `og_type`, `og_url`, `twitter_card`,
`twitter_title`, `twitter_description`, `twitter_image`, `robots` — and values
must be string literals; unknown or repeated keys are compile errors. Handlers
receive the declared values by taking a `SeoMeta` parameter (it implements
`FromRequestParts` and never fails; a route without `seo(...)` yields an empty
builder), then refine them with per-request data before calling
`seo.render()`. `#[static_get]` honours the argument too. The declared values
are also recorded on `Route::seo` as a `SeoRouteDefaults`.

**Locale-prefixed routing** (**unreleased** — trunk-dev, #1251): `[i18n]
locale_prefix_enabled = true` in `autumn.toml` (default `false`, no behavior
change) makes every route registered via `routes(...)` also reachable under
`/{locale}/...` for each `supported_locales` entry — no route definitions are
duplicated. An unknown `{locale}` prefix 404s (never panics); a bare,
non-prefixed request 308-redirects to the negotiated locale's prefixed path,
preserving the query string. Inside a prefixed request the URL segment
outranks cookie/session/`Accept-Language` for the `Locale` extractor — no
handler changes needed. `[i18n] locale_prefix_exclude = ["/api", "/actuator"]`
keeps machine routes unprefixed. View helpers
`autumn_web::widgets::{localized_path(path, locale), locale_switcher(path,
current_locale, supported_locales)}` render path-and-query-preserving links
to the current page in each locale. `SeoMeta::hreflang_alternates(...)` +
`autumn_web::seo::locale_alternates(base_url, path, default_locale,
supported_locales)` emit `<link rel="alternate" hreflang="…">` tags (plus
`x-default`); `sitemap.xml` lists one entry per supported locale per eligible
static route when the flag is on.

`#[model]` also recognizes `#[belongs_to]` / `#[has_many]` / `#[has_one]`
struct-level attributes (**unreleased** — trunk-dev, not in published 0.5.0)
for declarative associations with batched eager
preloading (`Model::preload()`, `repo.preload(records, spec)`); these are
consumed by `#[model]` itself, not separately-registered proc macros.
`#[has_many(Target, through = join_table)]` declares a
many-to-many association through a join table, adding `add_{singular}` /
`remove_{singular}` / `set_{plural}` mutation helpers to the generated
`#[repository]`. See the `#[model]` doc comment in `autumn-macros/src/lib.rs`
and `docs/adr/0008-associations-and-eager-loading.md`.

`#[model]` also consumes the `#[state_machine(transitions(from -> to,
from -> to: "guard", ...))]` field attribute on `String` fields, generating
`can_transition_{field}_to(&self, &str) -> bool`,
`transition_{field}_to(&self, &str) -> AutumnResult<String>`, and a
`__AUTUMN_SM_{FIELD}_TRANSITIONS` edge-list constant. See
`docs/guide/state-machines.md`.

`#[model]` field attributes for column privacy and canonicalization
(**unreleased** — trunk-dev, not in published 0.5.0):

- `#[private]` (issue #1374) — excludes the column from the model's `Serialize`
  impl so it never appears in `Json` output, the auto-generated `--api`
  list/show endpoints, or any `serde_json::to_value(&model)`. The field stays a
  normal, queryable column and the **write** path is unaffected — `NewX` /
  `UpdateX` / `Changeset` still bind it, so a client can *set* a password but
  never read the hash back. `#[encrypted]` columns are `#[private]` in JSON by
  default (ciphertext/plaintext must not leak); opt back in with the existing
  `#[encrypted(admin_visible)]` knob. `#[private]` affects only serialization —
  the column still appears in `FormModel::form_fields()` (you must be able to
  *set* it). `autumn doctor` warns (`model_private_columns`) when a
  sensitively-named column (`password`, `token`, `secret`, `*_hash`) is not
  marked `#[private]`.
- `#[normalize(trim, downcase, upcase, squish, with = path::to::fn)]` (issue
  #1379) — canonicalizes a `String` column, composing normalizers
  left-to-right. Built-ins live in `autumn_web::normalize`
  (`trim`/`downcase`/`upcase`/`squish`); `with = path` calls a user
  `fn(&str) -> String`. Runs on the **write** path (`save`/`save_many` insert;
  `update` via `UpdateDraft::from_patch`) *before* the `before_create` /
  `before_update` hooks and the DB write, and on derived `#[repository]`
  `find_by_`/`count_by_` lookups (so `find_by_email("  FOO@X.com ")` matches the
  stored `foo@x.com` row). Built-ins are idempotent; composing
  `#[normalize(downcase)]` with a `unique` column yields case-insensitive
  uniqueness. Non-`String` fields are a compile error (mirrors `#[encrypted]`).
  Generated hooks: `impl autumn_web::normalize::Normalize` on the model and
  `NewX`; `impl autumn_web::normalize::NormalizedModel` (`normalize_lookup`) on
  every model.

## Repository-generated methods (`#[repository]`)

Published 0.5.0: `find_by_id`, `find_all`, `count`, `exists_by_id`, `save`,
`update`, `delete_by_id`, derived `find_by_*`/`count_by_*`/`exists_by_*`,
`page(&PageRequest)`, `cursor_page(&CursorRequest)` (with `cursor_key =` /
`cursor_key_type =` attr keys), bulk `save_many` / `save_many_skip_invalid` /
`update_many` / `delete_many` / `upsert_many` (compile error on hooked repos),
`with_lock`, `on_primary()`. Attr keys: `api =`, `policy =`,
`scope =`, `primary_reads`, `soft_delete`, `tenant_scoped`, `hooks =`,
`mcp` / `mcp = "read"`.

**(unreleased)**: `preload(records, spec)` (declarative associations);
`from_shard(&ShardedDb)`; `with_pool_untracked` (new on
trunk-dev — published 0.5.0 repositories have no pool constructor);
`find_in_batches(batch_size)` / `find_each(batch_size)` — bounded-memory
whole-table iteration on every repository via a primary-key keyset cursor
(`WHERE id > last ORDER BY id ASC LIMIT batch_size`, never `LIMIT`/`OFFSET`).
Handle types live in `autumn_web::batches`: `FindInBatches`
(`next_batch().await? -> Option<Vec<Model>>`), `FindEach`
(`next().await? -> Option<Model>`), and the macro-implemented `BatchSource`
trait. Inherits soft-delete/tenant-scoping/read-routing like `find_all`;
errors are retryable (keyset cursor advances only on success — `Ok(None)`
always means completion); `batch_size == 0` is an error; `batch_size` is not
clamped to `MAX_PAGE_SIZE`; sharded repositories reject cross-shard
`across_tenants()` iteration like `cursor_page`. See "Batched iteration" in
`docs/guide/pagination.md`.

**(unreleased)** — `find_or_create_by_<field>[_and_<field>...]`: declare
`fn find_or_create_by_slug(slug: String);` (lookup fields only) in the
`#[repository]` trait to generate an inherent
`find_or_create_by_slug(&self, slug: String, new: &NewModel) ->
AutumnResult<(Model, bool)>` — a race-safe get-or-insert returning the model
plus a `created` flag (#1382). Looks up on the read path first (replica-eligible,
tenant/soft-delete aware); if absent, inserts on the primary with `ON CONFLICT DO
NOTHING`, so under concurrency exactly one row is created, exactly one caller
sees `created == true`, and no `23505` unique-violation escapes (the loser
re-reads its own write and returns `(row, false)`). `before_create` /
`after_create` and the durable commit-hook queue fire only on the created path,
and — unlike `upsert_many` — the method IS generated on hooked repositories.
Requires a unique constraint covering the lookup column(s); `_or_` is rejected
(it would span constraints). See "Race-safe get-or-insert" in
`docs/guide/repositories.md`.

**(unreleased)** — typed grouped aggregate queries (#1364): declare
`fn count_grouped_by_<col>() -> Vec<(K, i64)>` or
`fn <sum|avg|min|max>_<num_col>_grouped_by_<col>() -> Vec<(K, Option<T>)>`
(`avg` → `Option<f64>`) in the `#[repository]` trait — the pair return type is
**required** (the macro reads the key/value SQL types from it). Each becomes an
inherent method returning a lazy `GroupedAggregate<'_, K, V>` builder that
yields one `(group, aggregate)` pair per group; nothing runs until the terminal
`.load().await -> AutumnResult<Vec<(K, V)>>`. Chain
`.order_by_aggregate_desc()` / `.order_by_aggregate_asc()` + `.limit(n)` for
top-N, `.filter_eq(v)` / `.filter_range(lo, hi)` to scope the group column
*before* aggregating (bound as params, inclusive range), or
`.bucket(autumn_web::aggregate::DateBucket::{Day,Week,Month})` for a
`date_trunc` time series keyed by bucket start. `sum`/`avg`/`min`/`max` are
null-safe (all-`NULL` group → `None`, empty table → empty `Vec`). Inherits
soft-delete + tenant scoping + read routing like `count`; a sharded,
tenant-scoped repo used via `across_tenants()` rejects the aggregate rather than
returning a per-shard-partial answer. `DateBucket` and the `GroupedAggregate`
builder live in `autumn_web::aggregate`. See "Grouped aggregate queries" in
`docs/guide/repositories.md`.

**(unreleased)** — `#[votable]` reaction helpers (#1362): declaring
`#[votable(by = User, aggregate = sum|count)]` on a `#[model]` emits a
`{Model}Reactions` trait blanket-implemented for that model's repository (no
`#[repository]` attribute needed — it rides the same `M2mConnSource<Model =
M>` bound as the m2m mutation helpers). Import the trait as `_`, then:

```rust
use crate::models::PostReactions as _;
use autumn_web::repository::{Reaction, ReactionOutcome};

// sum mode
async fn react(&self, reactor_id: i64, target_id: i64, value: i16)
    -> AutumnResult<Reaction>;
// count mode — no `value` parameter
async fn react(&self, reactor_id: i64, target_id: i64) -> AutumnResult<Reaction>;
// both modes
async fn reaction_of(&self, reactor_id: i64, target_id: i64)
    -> AutumnResult<Option<i16>>;

pub struct Reaction { pub value: Option<i16>, pub aggregate: i64,
                      pub outcome: ReactionOutcome }
pub enum ReactionOutcome { Inserted, Flipped, Removed }
```

`#[votable]` must be written **below** `#[model]` (attribute macros are
consumed top-down; above it, the error is `cannot find attribute votable`).
The model's `#[id]` and aggregate fields must both be `i64` — both are
compile-checked.

`react()` toggles (same value again removes the edge), flips (different value
replaces it), or inserts, and recomputes the target's aggregate column from
ground truth (`SUM(value)` / `COUNT(*)`) and persists it **in the same
transaction**, under a row lock on the target (`SELECT … FOR NO KEY UPDATE` on
Postgres — weak enough that referencing inserts, e.g. a comment on the same
post, are not blocked; `BEGIN IMMEDIATE` on SQLite) held across the whole
read-decide-write-recompute window. Concurrent callers therefore converge to
at most one edge per `(reactor, target)` with no `23505` escaping, and the
persisted aggregate is exact even across different reactors. A missing or
soft-deleted target is `NotFound`. Tenant-isolated: with a `tenant_id` column
on the model and a `tenant_scoped` repository, S1/S5 filter on the current
tenant, so a foreign-tenant `target_id` is `NotFound` (and `reaction_of`
returns `None`); no tenant context is an error, `across_tenants()` opts out.
**`react()` does not validate `value`** —
it writes whatever `i16` you pass, so branch on the route and put a
`CHECK (value IN (-1, 1))` on the column; never bind it from a request. A
toggle is not idempotent: a blind retry of a timed-out call inverts it, so use
an HTTP-layer idempotency key if you need retry safety. `reaction_of` is a
single indexed lookup on a **read** connection (replica-eligible, does not pin
read-your-writes — re-render from the `Reaction` the write returned; and no
batch form yet, so feed pages should pass `None` to the widget rather than
issue an N+1). **`react()` acquires its own pooled connection and does not
join an enclosing `Db::tx`** — never hold a `Db` extractor across the call, or
the handler needs two connections at once and deadlocks once concurrency
reaches the pool size. See `docs/guide/votable.md`.

**(unreleased)** — `ListQuery` extractor + `SortDir` (#1126): an `Infallible`
query extractor parsing `?sort=<col>`, `?dir=asc|desc`, and
`?filter[<col>]=<val>`. It **never rejects** — an empty or unknown `sort` falls
back to the model's default order, and an invalid `dir` falls back to `asc`
(`SortDir::{Asc, Desc}`). The `#[repository]`-generated `list(&ListQuery,
&PageRequest)` applies a typed per-column allowlist via Diesel `.into_boxed()`,
so only real columns can reach SQL (unknown sort/filter columns are ignored, not
injected).

## Db transactions

- `Db::tx(f)` — READ COMMITTED, one attempt (published 0.5.0).
- `Db::tx_with(opts: TxOptions, f) -> Result<T, AutumnError>`
  (**unreleased**) — closure gets `&mut AsyncPgConnection`; auto-retries
  SQLSTATE 40001 with capped exponential backoff.
- `autumn_web::db::IsolationLevel` {`ReadCommitted` (default),
  `RepeatableRead`, `Serializable`}; `TxOptions` builders
  `::read_committed()` / `::repeatable_read()` / `::serializable()` +
  `.read_only()` / `.deferrable()` / `.max_attempts(n)` /
  `.initial_backoff(d)` / `.max_backoff(d)`; retrying constructors default
  to 5 attempts.

## Form helpers (`autumn_web::form`)

Free functions rendering changeset-aware, accessible inputs:

- Published 0.5.0: `form_tag`, `method_input`, `text_input`,
  `text_input_htmx`; `Changeset`-bound methods (`form.form_tag(...)`,
  `form.text_input(...)`).
- **(unreleased)**: `checkbox_input`, `number_input`, `date_input`,
  `datetime_input`, `select_input`.
- **(unreleased — trunk-dev, not in published 0.5.0)**: `form_for(&changeset,
  action, method) -> FormFor` whole-form builder. Renders the opening
  `<form>` (audited CSRF injection + hidden `_method` override, as
  `form_tag`), one pre-filled control with inline errors per
  `FormModel::form_fields()` entry, and a submit button. `FormFor` builder
  methods: `.csrf(token)`, `.csrf_field_name(name)` (default `"_csrf"`),
  `.exclude(field)`, `.override_field(field, FieldControl)`,
  `.override_label(field, label)` (repeat calls on one field: last wins),
  `.append(markup)` (extra markup before the submit button),
  `.submit_label(label)` (default `"Save"`), `.multipart()`, `.render()`.
  `FormModel` (`fn form_fields() -> Vec<FormField>`) is implemented by
  `#[model]` for the same user-editable columns as `NewX`; hand-written
  impls build `FormField::new(name, label, control, required)` and use
  `.with_value_name(serialized_key)` when the data type serde-renames the
  field (the derive resolves `rename`/`rename_all` automatically —
  `value_name` affects only value pre-fill; input `name`/`id`, error
  lookup, and builder matching keep the Rust identifier).
  `FieldControl` variants (`#[non_exhaustive]`): `Text`, `Textarea`,
  `Password`, `Number { step: Option<String> }`, `Checkbox`, `Date`,
  `DateTime`, `Select { options: Vec<(String, String)> }`, `File` (any
  `File` field ⇒ `enctype="multipart/form-data"`).
  Decode-side contracts handled by the `#[model]`-generated `NewX`:
  non-nullable `bool` columns are `#[serde(default)]` (unchecked checkbox ⇒
  `false`); datetime columns attach `deserialize_datetime_local_utc[_option]`
  / `deserialize_datetime_local_local[_option]` /
  `deserialize_naive_datetime_local[_option]` (offsetless `datetime-local`
  values decode; RFC 3339 still accepted). `DateTime` columns with a zone
  other than `Utc`/`Local` render as `Text` (RFC 3339 string), not a picker.

## Typed accessible primitives (`autumn_web::a11y`, feature `maud`, unreleased — trunk-dev only, #1706)

Render-implementing structs that make the accessible name a **type-level
obligation** — the inaccessible form does not compile (the missing name is a
compile error enforced via trybuild, not a runtime `autumn a11y verify` miss).
Each maps to a WCAG 2.1 success criterion. Full narrative in
`docs/guide/accessibility.md`. `Img`, `Button`, `ButtonType`, `Link`,
`MenuItem`, and `TextField` are prelude re-exported; the remaining form
primitives (`TextArea`, `Select` + `SelectOption`, `Checkbox`, `FileField`) are
reached via the `autumn_web::a11y` path.

- `Img::new(src, alt)` (alt required) / `Img::decorative(src)` (explicit
  `alt=""` + `aria-hidden="true"`); `.class(..)` / `.width(u32)` /
  `.height(u32)`. WCAG 1.1.1.
- `Button::new(name)` (visible label) / `Button::icon(markup, name)`
  (name ⇒ `aria-label`); `.kind(ButtonType)` / `.submit()` / `.button()` /
  `.class(..)`. WCAG 4.1.2.
- `Link::new(href, text)` / `Link::icon(href, markup, name)`; `.new_tab()`
  (`target="_blank"` + `rel="noopener noreferrer"`) / `.class(..)`. WCAG 2.4.4 /
  4.1.2.
- `MenuItem::new(name)` (renders `role="menuitem"` on a `<button>`; `.href(..)`
  switches to `<a>`); `.icon(markup)` (name ⇒ `aria-label`) / `.class(..)`.
  WCAG 4.1.2.

### Labeled-typestate form primitives (`TextField` / `TextArea` / `Select` / `Checkbox` / `FileField`)

Each starts in a `NoLabel` typestate that does **not** implement `Render`. Only
after a label is attached — `.label(text)` (visible `<label for=…>`),
`.aria_label(text)`, or `.labelled_by(id)` (`aria-labelledby`) — does it become
the `Labeled` state, the only one that renders (e.g. `TextField::new(name)`
returns `TextField<NoLabel>` → `TextField<Labeled>`). So an unlabeled field is
unrepresentable as markup (WCAG 1.3.1 / 3.3.2 / 4.1.2). Presentational and
validation setters are chainable in **either** typestate and none supplies an
accessible name, so none lifts the compile-time label obligation — the guarantee
is additive.

Shared setters on all five: `.required()` (native `required`),
`.aria_required()` (mirroring `aria-required="true"`, matching the scaffold
generator's non-nullable ARIA wiring), `.class(s)` (on the control),
`.label_class(s)` (on the visible `<label>`), `.aria_invalid(bool)`
(`aria-invalid="true"`/`"false"`; omitted entirely when unset),
`.described_by(id)` (`aria-describedby` referencing an error container's `id` so
AT announces the error with the input), and — the **`.hx(name, value)` escape
hatch** — an arbitrary `hx-*` attribute (the `name` is the suffix after `hx-`),
emitted in insertion order, preserving the typed label obligation.

Per-primitive setters (in addition to the shared set):

- **`TextField::new(name)`** — `.input_type(s)` (e.g. `"email"`, `"number"`),
  `.value(s)`, `.minlength(u32)` / `.maxlength(u32)` (scaffold
  `text{min=…}` / `{max=…}`), `.min(s)` / `.max(s)` (HTML5 numeric bounds,
  passed through verbatim), `.step(s)` (e.g. `"any"` for float fields).
- **`TextArea::new(name)`** — `.value(s)`, `.minlength(u32)` /
  `.maxlength(u32)`, `.rows(u32)` / `.cols(u32)`.
- **`Select::new(name)`** — `.option(value, label)` / `.options(iter of
  SelectOption)` (`SelectOption::new(value, label)`), `.selected_value(s)`.
- **`Checkbox::new(name)`** — `.value(s)`, `.checked(bool)`.
- **`FileField::new(name)`** — `.accept(s)` (MIME/extension filter),
  `.multiple()` (sets the `multiple` attribute).

## View widgets and UI (all unreleased — trunk-dev only)

- `autumn_web::widgets`: `card(&body, &CardConfig)`,
  `stat_card(label, value, link)`, `tabs(id, &[(id, label, markup)])`,
  `modal(id, title, &body, &ModalConfig)`, `modal_trigger`,
  `modal_close_button`, `confirm_action(...)`.
- `autumn_web::widgets::transition_controls(action, field, current,
  transitions, can, csrf, csrf_field)` (#1917) — renders one CSRF-protected,
  no-JS `POST` form + submit button per field-level `#[state_machine]`
  transition whose `from == current` (a terminal state renders an empty
  `role="group"` container). `transitions` is the macro-generated
  `Model::__AUTUMN_SM_<FIELD>_TRANSITIONS` constant (`&[(from, to,
  Option<guard>)]`) and `can` is `|to| record.can_transition_<field>_to(to)`;
  a legal edge whose guard currently fails still renders but as a `disabled`
  button. CSS hooks `.autumn-transition-controls` / `.autumn-transition`.
- `autumn_web::widgets::{ReactionControls, reaction_controls}` (#1362) — the
  view half of `#[votable]`. `ReactionControls::votes(dom_id, up_action,
  down_action)` (signed up/down, `aggregate = sum`) or
  `ReactionControls::likes(dom_id, action)` (single toggle, `aggregate =
  count`), then `.aggregate(i64)` (the target's persisted `score` /
  `{name}_count`), `.current(Option<i16>)` (`reaction_of()`'s result —
  `Some(1)` presses up/like, `Some(-1)` down, `None` neither), `.label(s)` /
  `.up_label(s)` / `.down_label(s)` / `.like_label(s)` (accessible names),
  `.hx_target(s)` (defaults `#{dom_id}`), `.csrf(Option<&CsrfToken>,
  Option<&CsrfFormField>)` or the `.csrf_token(s)` / `.csrf_field(s)`
  primitives; render with `reaction_controls(&cfg) -> Markup`. Emits one
  no-JS `<form method="post">` per direction carrying `hx-post` / `hx-target`
  / `hx-swap="outerHTML"`, ARIA toggle buttons (`aria-pressed`, explicit
  `aria-label`, glyph in `aria-hidden` span) and an `aria-live="polite"`
  aggregate. Thread `.csrf(...)` on any page a no-JS visitor can reach — the
  hidden input is what makes the plain form POST pass CSRF (the htmx path is
  covered by the header shim either way). `dom_id` is interpolated into the
  default `hx-target` selector, so build it yourself; never nest the widget
  inside another `<form>`. CSS hooks `.autumn-reaction-controls` / `.autumn-reaction` /
  `.autumn-reaction-up` / `.autumn-reaction-down` / `.autumn-reaction-like` /
  `.autumn-reaction-button` / `.autumn-reaction-active` /
  `.autumn-reaction-count`. Prelude re-exported.
- `autumn_web::widgets` display atoms: `badge(label, BadgeVariant)` /
  `badge_with(..., &BadgeConfig)` / `status_tag(label)` with
  `BadgeVariant::{Neutral,Info,Success,Warning,Danger}` and
  `BadgeVariant::for_label(&str)` (deterministic color); `avatar(name,
  &AvatarConfig)` with `AvatarSize::{Small,Medium,Large}` (image or
  colored-initials fallback); `alert(AlertVariant, body)` / `alert_with(...,
  &AlertConfig)` with `AlertVariant::{Info,Success,Warning,Error}` and
  `error_summary(&Changeset) -> Option<Markup>`. All prelude re-exported.
- `autumn_web::widgets` feedback atoms: `toast(message, AlertVariant)` /
  `toast_in(region_id, message, AlertVariant)` / `toast_region(id)` +
  `DEFAULT_TOAST_REGION_ID` — transient htmx toast appended out-of-band
  (`hx-swap-oob="beforeend:#<region-id>"`), CSS-only auto-dismiss (no JS).
  `toast_region` is a persistent `aria-live="polite"` region; non-error toasts
  inherit its politeness (no own `role`/`aria-live`) while `Error` announces
  assertively via its own `role="alert"` (reuses the `AlertVariant` color
  lane). `infinite_feed(items, next_cursor, &FeedConfig)` /
  `feed_page(items, next_cursor, &FeedConfig)` with `FeedMode::{Reveal,Button}`
  — htmx infinite-scroll / "Load more" feed from a `CursorPage`: one cursored
  `hx-get` sentinel appends the next page in place (no reload, no duplicate
  rows), progressive `<a href>` fallback; `feed_page` is the per-page append
  fragment. All prelude re-exported.
- `autumn_web::flash::{flash_messages, flash_messages_with,
  FlashMessagesConfig}` — accessible flash-banner renderer (per-severity
  `role`/`aria-live`, `autumn-flash--<level>` classes, empty renders nothing,
  optional no-JS dismiss). Prelude re-exported.
- `autumn_web::links`: `link_to`, `link_to_with`, `button_to(label, href,
  Method, csrf_token)`, `button_to_with(..., &ButtonToOptions)`.
- `autumn_web::ui::pagination`: `pagination_nav(&Page, &PagerOptions)`,
  `cursor_pagination_nav(&CursorPage, &PagerOptions)`, `PagerOptions::new(base)
  .query(qs).hx_target(sel).hx_push_url()` (prelude re-exports).
- `autumn_web::ui::{WIDGETS_CSS, WIDGETS_CSS_PATH}` widget stylesheet.
- `autumn_web::consent` (issue #1214, `maud`-gated parts noted below) —
  cookie-consent banner + gate scaffolded by `autumn new` by default.
  `Consent` extractor (reads the `Cookie` header directly, no middleware
  needed) with `consent.allows(category, current_policy_version) -> bool`
  (always `true` for `"necessary"`) and `consent.needs_prompt(current_policy_version)
  -> bool`. `accept_all_cookie(&[categories], policy_version)` /
  `reject_non_essential_cookie(policy_version)` / `expire_consent_cookie()`
  build the `Set-Cookie` value (categories + policy version + RFC 3339
  timestamp). `consent_banner_markup(csrf_token, csrf_field_name)` (feature
  `maud`) and `inject_consent_banner(request, next, policy_version,
  csrf_cookie_name, csrf_form_field)` (feature `maud`, `DEFAULT_CSRF_COOKIE_NAME`
  / `DEFAULT_CSRF_FORM_FIELD` constants for the unconfigured defaults) — a
  response-body-splice middleware, registered via
  `.layer(axum::middleware::from_fn(...))`, that auto-injects the banner into
  every HTML response without changing the shared `layout()` signature.
  `csrf_form_field` is a plain parameter (not read from a `CsrfFormField`
  request extension) because `CsrfLayer` always sits inner to user layers in
  the documented stack. An internal `autumn build` / ISR render (tagged
  `static_gen::RenderDeadlineExempt`) is passed through untouched rather than
  having the banner baked into the static file on disk. `safe_redirect_target`
  / `redirect_target_from_referer` are open-redirect-safe helpers for routing
  a visitor back to the page they were on after they record a choice. Session
  and CSRF cookies are never routed through the gate. Known limitation: a
  first-time visitor whose first hit lands on a `#[static_get]` page is served
  before `CsrfLayer` runs, so the banner has no CSRF token to embed and its
  forms 403 until the visitor reaches a dynamic page at least once.

(Typed accessible primitives — `Img` / `Button` / `Link` / `MenuItem` /
`TextField` / `TextArea` / `Select` / `Checkbox` / `FileField` — are documented
in **Typed accessible primitives** above.)

## Cache read-through (unreleased)

`autumn_web::cache::{get_or_compute, get_or_compute_with,
GetOrComputeOptions, CacheFillError, jittered_ttl}` — single-flight fills,
optional `.distributed_fill_lock(true)` / `.stale_while_revalidate(grace)`.

## Downloads (unreleased)

`autumn_web::download::Download` — a typed file-download `IntoResponse`.

- Constructors: `Download::from_bytes(impl Into<Bytes>)` (sets
  `Content-Length`), `Download::from_stream(stream)` (an async
  `Stream<Item = Result<Bytes, std::io::Error>>`), `Download::from_async_read(reader)`
  (any `tokio::io::AsyncRead`), and `Download::from_blob(&store, key).await?`
  (streams a stored blob via `BlobStore::get_stream` without buffering; sets
  `Content-Length` and a default content-type from blob metadata; requires the
  `storage` feature).
- Setters (chained, `#[must_use]`): `.filename(name)`, `.content_type(ct)`,
  `.inline()` (defaults to `attachment`).
- Sets `Content-Disposition` (RFC 5987 `filename*=UTF-8''…` for non-ASCII
  names, sanitized against header injection), resolves `Content-Type` in the
  order explicit `.content_type()` → filename extension → blob metadata →
  `application/octet-stream`, and sets `Content-Length` when known.
- One-expression example (behind `#[secured]`):
  `Ok(Download::from_blob(&store, key).await?.filename("report.pdf"))`.
- Additive `BlobStore::get_stream(key) -> ByteStream<'static>` streams an
  object's bytes; `LocalBlobStore` overrides it to stream from disk, other
  backends inherit a buffering default.

### Range / 206 Partial Content (unreleased)

`autumn_web::range` — reusable HTTP `Range` (RFC 7233) parsing + response
building, wired into `Download` and the embedded static-asset path.

- `range::resolve(&HeaderMap, total, Option<Validator>) -> RangeResolution`
  parses `Range: bytes=N-` / `N-M` / `-N` against a known `total`, returning
  `RangeResolution::{Full, Partial{start,end,total}, Unsatisfiable{total}}`
  (`start`/`end` inclusive). Invalid/unparseable ranges and non-`bytes` units
  return `Full` (serve the whole representation with `200`).
- `range::partial_bytes_response(&RangeResolution, Bytes)` builds the response:
  `206` + `Content-Range: bytes start-end/total` + `Content-Length` for a
  partial, `200` + `Accept-Ranges: bytes` for full, `416` +
  `Content-Range: bytes */total` for unsatisfiable. Helpers:
  `content_range_value`, `unsatisfied_content_range`, `set_accept_ranges`.
- **Multi-range** (`bytes=0-50,100-150`) is collapsed deterministically to the
  first satisfiable sub-range as a well-formed single-range `206` (no
  `multipart/byteranges`).
- **`If-Range`** (strong `ETag` or `Last-Modified` HTTP-date) via `Validator`:
  a stale/absent validator falls back to the full `200`.
- `Download::into_response_ranged(&headers).await` is the request-aware entry
  point that returns `206`/`416` and advertises `Accept-Ranges: bytes` on the
  `200`/`206`/`416` for range-capable bodies. The plain `IntoResponse` cannot
  see the request, always serves the full `200`, and therefore does **not**
  advertise `Accept-Ranges` (only `into_response_ranged` can honor a `Range`).
  Add `.etag(..)` / `.last_modified(..)` to supply the `If-Range` validator.
  Opaque `from_stream`/`from_async_read` bodies are not seekable: always full
  `200`, never `Accept-Ranges`.
- Blob range path fetches only the requested slice via the additive
  `BlobStore::get_range(key, start, end) -> ByteStream<'static>`
  (`LocalBlobStore` seeks + takes off disk; other backends inherit a buffering
  default) — a seek in a large video never buffers the whole object.

## PDF generation (unreleased)

`autumn_web::pdf::Pdf` (`pdf` Cargo feature, off by default) — renders an
HTML string, typically a `maud::Markup` view you already render on-screen,
to a downloadable PDF `IntoResponse` built on `Download`.

- Constructors: `Pdf::from_html(impl Into<String>)`, and (with the `maud`
  feature) `Pdf::from_markup(maud::Markup)`.
- Setters (chained, `#[must_use]`): `.filename(name)` (defaults to
  `document.pdf`, RFC 6266-safe via the same sanitization as
  `Download::filename`), `.inline()` (defaults to `attachment`).
- `.render() -> Vec<u8>` renders to raw bytes without building a response —
  for emailing an invoice attachment, writing to a `Blob` store, or a test.
- Supported HTML subset: `h1`-`h6`, `p`, `table`/`tr`/`th`/`td`,
  `ul`/`ol`/`li`, `strong`/`b`, `em`/`i`, `br`, `hr` — flowed top-to-bottom in
  a single column with the PDF base-14 fonts (no CSS box model, not
  pixel-perfect by design). Any other tag (`div`, `span`, `a`, widget
  markup, ...) passes its text through transparently instead of being
  dropped or erroring.
- No system-installed browser/renderer and no embedded font files at
  runtime — keeps the single-binary story intact (issue #1004).
- Determinism: identical HTML input always produces identical extracted
  text (nothing reads the wall clock internally — feed a timestamp through
  the `Clock` extractor into the HTML yourself if you need one). Raw bytes
  are not guaranteed byte-identical (`printpdf` assigns a random trailer
  `/ID` per the PDF spec, not configurable).
- `autumn_web::pdf::extract_text(&[u8]) -> Result<String, String>` reads a
  PDF's visible text back out via `printpdf`'s own parser.
- `TestResponse::assert_pdf_contains(&self, substring: &str) -> &Self` — test
  helper built on `extract_text`, alongside `assert_body_contains`.
- See `docs/guide/pdf-downloads.md` and the `examples/invoice` worked example.

## Jobs additions

- Published 0.5.0 `#[job]` keys: `name`, `max_attempts`, `backoff_ms`,
  `unique`, `unique_by`, `unique_window`, `unique_for_ms`, `concurrency`,
  `concurrency_key`.
- **(unreleased)**: `queue = "name"` + `[jobs] queues` strict-priority list or
  `[jobs.queues]` weight table; tracked jobs (`job::enqueue_tracked`,
  `enqueue_tracked_for`, `TrackedJobHandle`, optional third `JobContext`
  handler arg, `GET /_autumn/jobs/{token}`, `jobs.tracking.*` config).

## Distributed locks (unreleased — trunk-dev)

- `autumn_web::lock::Lock` (prelude: `Lock`, `LockGuard`, `LockError`; `db`
  feature). Named, cluster-wide Postgres advisory lock for
  run-once-across-replicas work.
- Build: `Lock::new(pool, "name")`, `Lock::from_state(&state, "name")` (primary
  pool), `.with_poll_interval(dur)`. Key helper: `distributed_lock_key(name) ->
  i64` (namespaced under `DISTRIBUTED_LOCK_DOMAIN = "autumn:lock:v1"`).
- Acquire: `try_lock() -> Option<LockGuard>`, `lock()`, `lock_timeout(dur)`
  (`LockError::Timeout`). Closures: `with(f)`, `with_timeout(dur, f)`,
  `try_with(f) -> Option<T>`. For run-once (must-not-run-twice) work use
  `try_with`/`try_lock` and skip on `None`; blocking `with`/`with_timeout`
  *serialize* — every waiter eventually runs, so they are not run-once.
  Auto-releases on scope end / `?` / panic;
  `LockGuard::release().await` releases explicitly. `LockError::PoolUnavailable`
  / `Timeout` map to `503`.
- Non-goals: not fair, not a lease, not row-level (`with_lock`), Postgres only.

## Auth additions

- Published 0.5.0: `autumn generate auth` session management (`{user}_sessions`
  table, `sessions()` / `revoke_session` / `revoke_other_sessions` /
  `revoke_all_sessions`, `/account/sessions` page, `[auth.sessions]` config).
- **(unreleased)**: scoped service tokens — `IssueTokenSpec`,
  `issue_scoped_api_token`, `#[secured(scopes = [...])]`,
  `PolicyContext::has_scope/has_any_scope/has_all_scopes`, `autumn token
  issue --name/--scope/--expires-at | list | rotate`, admin `TokenAdminModel`.

## Submit tokens (unreleased — trunk-dev, #1360)

One-time, at-most-once form submission with no JS — defends against
double-submits and replays.

- `SubmitToken` extractor, `SubmitFormField` (renders the hidden
  `_submit_token` field), and `SubmitTokenLayer` middleware.
- On submit the token is consumed against the idempotency store: a first
  submission runs the handler, a **replay** returns the cached response, and a
  **concurrent duplicate** in flight gets `409`.
- Config `[security.submit_token]`: `enabled` (default `true`), `field_name`
  (default `_submit_token`), `ttl_secs` (default `600`), `in_flight_ttl_secs`
  (default `86400`), `backend` (inherits `[idempotency].backend`; an inherited
  in-memory backend in prod warns, while an explicit `memory` backend in prod
  fails fast), `exempt_paths`.
- The layer is applied inner to CSRF.

## Prelude contents

`use autumn_web::prelude::*;` includes:

- Route macros: `get`, `post`, `put`, `patch`, `delete`, `routes`, `main`,
  `static_get`, `static_routes`, `scheduled`, `tasks`, `job`, `jobs`, `task`,
  `one_off_tasks`, `secured`, `authorize`, `service`, `cached`, `api_doc`,
  `oauth2_callback`, `paths`, `step_up`, `ws` (when `ws` feature enabled).
  **Note**: `#[model]` and `#[repository]` are NOT in the prelude — use
  `#[autumn_web::model]` and `#[autumn_web::repository]` (qualified paths).
- Rendering: `asset_url`, `Markup`, `PreEscaped`, `html!`.
- Accessibility primitives (`maud` feature, **unreleased** — trunk-dev):
  `Button`, `ButtonType`, `Img`, `Link`, `MenuItem`, `TextField`.
- Extractors: `Db`, `Form`, `Json`, `Path`, `Query`, `State`, `Session`,
  `Auth`, `ApiToken`, `RequireApiToken`, `CsrfToken`, `CsrfFormField`,
  `PageRequest`, `Page`, `CursorRequest`, `CursorPage`, `Valid`,
  `ValidateExt`, `Validated`, `Flash`, `Multipart`, `HxRequest`,
  `HxResponseExt`, `Sse`, `Event`, `TaskArgs`, `SignedWebhook`.
- Error and response: `AutumnError`, `AutumnResult`, `IntoResponse`,
  `StatusCode`, `Redirect`.
- Data helpers: `Changeset`, `ChangesetForm`, `IntoChangeset`, mutation hook
  types, authorization `Policy`, `PolicyContext`, `Scope`, `ScopeQuery`,
  `Scoped`.
- State and infrastructure: `AppState`, broadcast/channel types, mail types,
  webhook config helpers, `Locale` and `t!` when enabled.

## AppBuilder methods

| Method | Notes |
|---|---|
| `routes(Vec<Route>)` | Main route registration |
| `static_routes(Vec<StaticRouteMeta>)` | Static pre-render metadata |
| `tasks(Vec<TaskInfo>)` | Scheduled tasks |
| `jobs(Vec<JobInfo>)` | Background jobs |
| `one_off_tasks(Vec<OneOffTaskInfo>)` | CLI tasks |
| `migrations(EmbeddedMigrations)` | Diesel embedded migrations |
| `openapi(OpenApiConfig)` | OpenAPI generation |
| `mount_mcp(path)`, `expose_all_as_mcp()`, `secure_mcp(layer)` | MCP endpoint projection (`mcp`); `Route::mcp()/mcp_exclude()/mcp_stream()` toggle exposure per route (plugin fluent opt-in) |
| `exception_filter(...)`, `error_pages(...)` | Error rendering |
| `scoped(prefix, layer, routes)` | Scoped route group |
| `layer(...)`, `has_layer<T>()`, `get_layer_types()` | Tower middleware |
| `merge(router)`, `nest(path, router)` | Raw Axum composition |
| `declare_plugin_routes(...)` | Plugin route declarations |
| `on_startup(...)`, `on_shutdown(...)` | Lifecycle hooks |
| `with_extension(value)`, `update_extension(...)`, `extension<T>()` | Typed state extensions |
| `i18n(bundle)`, `i18n_auto()` | I18n bundle setup |
| `with_config_loader(loader)` | Replace config loading |
| `with_pool_provider(provider)` | Replace DB pool creation |
| `with_telemetry_provider(provider)` | Replace telemetry setup |
| `with_session_store(store)` | Replace sessions |
| `with_channels_backend(backend)` | Replace channels |
| `with_blob_store(store)` | Install storage |
| `with_cache_backend(cache)` | Install cache |
| `with_mail_delivery_queue(queue)` / `with_mail_delivery_queue_factory(...)` | Durable mail |
| `mail_previews(...)` | Dev mail previews |
| `with_audit_sink(sink)` | Structured audit sink |
| `policy::<R, P>(policy)`, `scope::<R, S>(scope)` | Repository authorization |
| `plugin(plugin)`, `plugins(tuple)` | Plugin install |
| `listeners(listeners![...])` | Event listeners (**unreleased**) |
| `static_gate(layer)`, `has_static_gate::<L>()`, `get_static_gate_types()` | Static pre-render gating middleware (**unreleased**) |
| `with_shard_router(router)` | Sharding router (**unreleased**) |
| `run()` | Start server |

## Cargo features

```toml
[features]
default = ["maud", "htmx", "tailwind", "db", "cache-moka", "http-client", "reporting"]
ws = ["dep:tokio-stream"]
flash = []
cache-moka = ["dep:moka"]
maud = ["dep:maud"]
htmx = []
multipart = ["axum/multipart"]
tailwind = []
oauth2 = ["http-client"]
http-client = ["dep:reqwest"]
openapi = ["dep:serde_yaml"]
mcp = ["openapi"]
markdown = ["dep:pulldown-cmark"]
db = [
    "dep:deadpool",
    "dep:diesel",
    "dep:diesel-async",
    "dep:diesel_migrations",
    "dep:libsqlite3-sys",
    "dep:pq-sys",
    "dep:scoped-futures",
    "dep:tokio-postgres",
    "diesel/postgres",
    "diesel/chrono",
]
test-support = ["dep:testcontainers", "dep:testcontainers-modules"]
telemetry-otlp = [
    "dep:opentelemetry",
    "dep:opentelemetry_sdk",
    "dep:opentelemetry-otlp",
    "dep:tracing-opentelemetry",
]
redis = ["dep:redis"]
i18n = []
storage = ["diesel?/serde_json"]
mail = ["dep:lettre", "maud"]
seed = ["db"]
system-info = []
reporting = []
webauthn = ["dep:webauthn-rs"]
csv = ["dep:csv"]
system-tests = ["dep:chromiumoxide"]
```

`storage-s3` is not a feature in 0.5.0. Use `autumn-storage-s3 = "0.5"`.

## Workspace dependency versions

```toml
axum = { version = "0.8", features = ["macros", "ws"] }
tokio-util = "0.7"
diesel = { version = "2", features = ["sqlite", "postgres"] }
pq-sys = { version = "0.7", features = ["bundled_without_openssl"] }
diesel-async = { version = "0.8", features = ["deadpool", "postgres"] }
diesel_migrations = "2"
http = "1"
libsqlite3-sys = { version = "0.36", features = ["bundled"] }
tokio = { version = "1", features = ["full"] }
tokio-stream = { version = "0.1", features = ["sync"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
thiserror = "2"
maud = { version = "0.27", features = ["axum"] }
toml = "1.1"
tower = "0.5"
tower-http = { version = "0.6", features = ["cors", "fs", "trace", "compression-gzip", "compression-br"] }
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["json", "env-filter"] }
tracing-opentelemetry = "0.32.1"
opentelemetry = { version = "0.31.0", default-features = false, features = ["trace"] }
opentelemetry_sdk = { version = "0.31.0", default-features = false, features = ["trace"] }
opentelemetry-otlp = { version = "0.31.0", default-features = false, features = ["trace", "grpc-tonic", "http-proto", "reqwest-client"] }
redis = { version = "1.2.0", default-features = false, features = ["aio", "tokio-comp", "connection-manager", "script"] }
tokio-cron-scheduler = { version = "0.15", features = ["signal"] }
chrono-tz = "0.10"
validator = { version = "0.20", features = ["derive"] }
bcrypt = "0.19"
futures = "0.3"
indexmap = "2"
moka = { version = "0.12", features = ["sync"] }
chrono = { version = "0.4", features = ["serde"] }
testcontainers = "0.27"
testcontainers-modules = { version = "0.15", features = ["postgres", "redis", "minio"] }
time = { version = ">=0.3, <0.4" }
```

## Error constructors

`AutumnError` provides status-aware constructors:

- `internal_server_error(err)` / `internal_server_error_msg(msg)` - 500
- `not_found(err)` / `not_found_msg(msg)` - 404
- `bad_request(err)` / `bad_request_msg(msg)` - 400
- `unprocessable(err)` / `unprocessable_msg(msg)` - 422
- `service_unavailable(err)` / `service_unavailable_msg(msg)` - 503
- `unauthorized(err)` / `unauthorized_msg(msg)` - 401
- `forbidden(err)` / `forbidden_msg(msg)` - 403
- `conflict(err)` / `conflict_msg(msg)` - 409
- `validation(details)` - 422 with field errors

JSON clients receive `application/problem+json`.

## Signed webhook API

Provider presets:

- `WebhookProvider::Stripe`
- `WebhookProvider::Github`
- `WebhookProvider::Slack`
- `WebhookProvider::Generic`

Endpoint builders:

- `WebhookEndpointConfig::new(name, path, provider, secret)`
- `WebhookEndpointConfig::stripe(name, path, secret)`
- `WebhookEndpointConfig::github(name, path, secret)`
- `WebhookEndpointConfig::slack(name, path, secret)`
- `WebhookEndpointConfig::generic(name, path, secret)`
- `.with_previous_secret(secret)`
- `.with_timestamp_tolerance_secs(secs)`
- `.with_replay_window_secs(secs)`

`SignedWebhook` methods:

- `provider() -> &'static str`
- `endpoint() -> &str`
- `delivery_id() -> Option<&str>`
- `event_type() -> Option<&str>`
- `received_at() -> SystemTime`
- `raw_body() -> &[u8]`
- `json<T>() -> Result<T, serde_json::Error>`

## Feeds (Atom/RSS)

`autumn_web::feed` renders syndication feeds and returns them straight from a
`#[get]` handler:

- `feed::Feed` — channel builder. `Feed::atom(title, site_link, self_link)` and
  `Feed::rss(title, site_link, self_link)` (same signature) pick the format;
  chain `.author(..)`, `.description(..)`, `.updated(DateTime<Utc>)`,
  `.entry(FeedEntry)`, `.entries(iter)`.
- `feed::FeedEntry` — per-item builder. `FeedEntry::new(id, title, link)` plus
  `.summary(..)`, `.content(..)`, `.published(DateTime<Utc>)`,
  `.updated(DateTime<Utc>)`.
- `Feed` implements `IntoResponse`, setting `Content-Type: application/atom+xml`
  (Atom) or `application/rss+xml` (RSS), UTF-8, and XML-escaping every text
  field. `Feed::render() -> String` returns the raw XML.
- `Feed::conditional(&headers) -> Response` reuses the `etag` module: it
  computes the feed's ETag (also available via `Feed::etag()`) and returns a
  `304 Not Modified` when the request's `If-None-Match` matches, otherwise the
  full `200` feed. See `docs/guide/conditional-get.md`. The `blog` example
  wires a `/feed.xml` route this way.

## Cache-Control freshness (`etag::cache_for` / `CacheControl`)

Declarative per-handler `Cache-Control` header (unreleased — issue #1344).
While `fresh_when` handles *revalidation* (is a cached copy still valid?),
`cache_for` handles *freshness* (how long may a copy be reused before
revalidating?). Both are re-exported from the prelude.

- `etag::cache_for(ttl: Duration) -> CacheControl` — starts a directive with
  `max-age=ttl`, defaulting to `private`.
- Attach it two ways: as a tuple with any body via `IntoResponseParts` —
  `(cache_for(dur).public(), html!{ … })` — or `CacheControl::wrap(response)`
  for a single expression. Either way the header is **inserted** (replacing any
  prior value), so exactly one `Cache-Control` is emitted.
- Chainable directives: `public()` / `private()`, `max_age(d)`, `s_maxage(d)`,
  `stale_while_revalidate(d)`, `no_store()`, `no_cache()`, `must_revalidate()`,
  `immutable()`. Durations render as whole seconds.
- `CacheControl::header_value() -> String` renders the deterministic,
  byte-for-byte value. Ordering: `no-store` alone if set, otherwise visibility,
  `no-cache`, `max-age`, `s-maxage`, `stale-while-revalidate`,
  `must-revalidate`, `immutable`.
- **`max-age` vs `s-maxage`**: `max-age` applies to every cache (browser
  included); `s-maxage` overrides it for **shared** caches (CDN/proxy) only.
- **`Vary`**: only mark a personalized page `public` alongside a matching
  `Vary` (e.g. `Vary: Cookie`) so a shared cache never serves one user's page
  to another — otherwise keep it `private`/`no_store`.
- **Default-private safety**: `public` is an explicit opt-in, so dropping
  `cache_for(..)` onto a secured/authenticated handler can't silently publish
  it to a shared cache.
- Composes with `fresh_when`:
  `fresh_when(&headers, etag).or(cache_for(dur).public().wrap(markup))` — the
  freshness directives ride the `200` and the preserved `304`. See
  `docs/guide/conditional-get.md`.

## Config layering and env keys

Layering order, lowest to highest:

1. framework defaults
2. profile smart defaults
3. `autumn.toml`
4. `[profile.<name>]` in `autumn.toml`
5. `autumn-{profile}.toml`
6. `AUTUMN_*` env vars

Profile selection precedence:

1. `AUTUMN_ENV`
2. `AUTUMN_PROFILE`
3. `--profile <name>`
4. `AUTUMN_IS_DEBUG` auto-detection from the macro

`.env` auto-loading (unreleased — trunk-dev): a project-root `.env` file is a
local-dev feeder for the env-var layer (6), not a new precedence tier. On
startup Autumn injects `.env` values into the process environment only for keys
that are still unset, so a real environment variable of the same name always
wins. Files load in order `.env` → `.env.local` → `.env.{profile}` →
`.env.{profile}.local`, and earlier files (and real env vars) win. Auto-loaded
in `dev`/`test`; skipped in `prod` unless `AUTUMN_DOTENV=1` (set `AUTUMN_DOTENV=0`
to disable it anywhere). A malformed `.env` fails loudly at startup.

Frequently used env keys:

| Env | Config field |
|---|---|
| `AUTUMN_DATABASE__PRIMARY_URL` | `database.primary_url` |
| `AUTUMN_DATABASE__REPLICA_URL` | `database.replica_url` |
| `AUTUMN_DATABASE__REPLICA_FALLBACK` | `database.replica_fallback` |
| `AUTUMN_DATABASE__AUTO_MIGRATE_IN_PRODUCTION` | `database.auto_migrate_in_production` |
| `AUTUMN_SESSION__BACKEND` | `session.backend` |
| `AUTUMN_SESSION__REDIS__URL` | `session.redis.url` |
| `AUTUMN_CHANNELS__BACKEND` | `channels.backend` |
| `AUTUMN_CHANNELS__REPLAY_BUFFER` | `channels.replay_buffer` (unreleased — trunk-dev) |
| `AUTUMN_JOBS__BACKEND` | `jobs.backend` |
| `AUTUMN_JOBS__REDIS__URL` | `jobs.redis.url` |
| `AUTUMN_SCHEDULER__BACKEND` | `scheduler.backend` |
| `AUTUMN_SECURITY__SIGNING_SECRET` | `security.signing_secret.secret` |
| `AUTUMN_SECURITY__ALLOW_UNAUTHORIZED_REPOSITORY_API` | `security.allow_unauthorized_repository_api` |
| `AUTUMN_SECURITY__WEBHOOKS__REPLAY__BACKEND` | `security.webhooks.replay.backend` |
| `AUTUMN_SECURITY__WEBHOOKS__REPLAY__REDIS__URL` | `security.webhooks.replay.redis.url` |
| `AUTUMN_MAIL__ALLOW_IN_PROCESS_DELIVER_LATER_IN_PRODUCTION` | `mail.allow_in_process_deliver_later_in_production` |
| `AUTUMN_STORAGE__BACKEND` | `storage.backend` |
| `AUTUMN_CACHE__BACKEND` | `cache.backend` |
| `AUTUMN_OBSERVABILITY__SERVER_TIMING` | `observability.server_timing` (unreleased — trunk-dev) — bool; `Server-Timing` response header opt-in. Defaults on in `dev`/`development`, off elsewhere. See `docs/guide/observability/server-timing.md`. |

### `[server.tls]` (feature `tls`, unreleased — trunk-dev, #1603)

In-process HTTPS termination on the same host:port (off by default).

- `cert_path`, `key_path` — PEM cert/key.
- `reload_interval_secs` (default `60`) — certs hot-reload by polling file
  mtimes.
- `handshake_timeout_secs` (default `10`).
- Fail-fast at startup on bad / missing / mismatched / expired PEM.

### `[server.tls.acme]` (feature `acme`, unreleased — trunk-dev, #1608)

Automatic ACME certificate provisioning + renewal; builds on `tls`, off by
default. Mutually exclusive with static `cert_path` / `key_path`.

- `domains` (required, non-wildcard), `contact_email` (required).
- `directory` — Let's Encrypt staging by default; `production` or a custom URL.
- `cache_dir` (default `config/acme`).
- `http_challenge_port` (default `80`).
- `renew_before_days` (default `30`, must be `< 90`).
- Automatic HTTP-01 provisioning + hourly leader-elected renewal. DNS-01 and
  wildcard certs are out of scope (#1620).

## reexports module

`autumn_web::reexports` exposes upstream crates for generated code and
downstream macro compatibility:

- `axum`
- `chrono`
- `diesel` and `diesel_async` with `db`
- `http`
- `tokio`
- `tokio_util`
- `tracing`
- `validator`

Proc macros should use `::autumn_web::reexports::*` instead of assuming direct
dependencies in downstream apps.
