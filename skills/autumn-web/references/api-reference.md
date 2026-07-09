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
- `Locale`, `t!` (`i18n`)
- OAuth2/OIDC config, provider presets, callback helpers, and identity values
  (`oauth2`)

## Proc macros

| Macro | Purpose |
|---|---|
| `#[get]`, `#[post]`, `#[put]`, `#[patch]`, `#[delete]` | HTTP route handlers |
| `routes![...]` | Collect route handlers |
| `#[autumn_web::main]` | Tokio runtime + Autumn profile bootstrap |
| `#[static_get]`, `static_routes![...]` | Static pre-render routes for `autumn build` |
| `#[ws]` | WebSocket route handler (`ws`) |
| `#[model]` | Diesel model derives (`db`) |
| `#[repository]` | CRUD repository and generated API (`db`); `mcp` / `mcp = "read"` expose the generated routes as MCP tools |
| `#[service]` | Service implementation scaffolding (`db`) |
| `#[secured]` | Session auth and role guard |
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

## View widgets and UI (all unreleased — trunk-dev only)

- `autumn_web::widgets`: `card(&body, &CardConfig)`,
  `stat_card(label, value, link)`, `tabs(id, &[(id, label, markup)])`,
  `modal(id, title, &body, &ModalConfig)`, `modal_trigger`,
  `modal_close_button`, `confirm_action(...)`.
- `autumn_web::widgets` display atoms: `badge(label, BadgeVariant)` /
  `badge_with(..., &BadgeConfig)` / `status_tag(label)` with
  `BadgeVariant::{Neutral,Info,Success,Warning,Danger}` and
  `BadgeVariant::for_label(&str)` (deterministic color); `avatar(name,
  &AvatarConfig)` with `AvatarSize::{Small,Medium,Large}` (image or
  colored-initials fallback); `alert(AlertVariant, body)` / `alert_with(...,
  &AlertConfig)` with `AlertVariant::{Info,Success,Warning,Error}` and
  `error_summary(&Changeset) -> Option<Markup>`. All prelude re-exported.
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

## Prelude contents

`use autumn_web::prelude::*;` includes:

- Route macros: `get`, `post`, `put`, `patch`, `delete`, `routes`, `main`,
  `static_get`, `static_routes`, `scheduled`, `tasks`, `job`, `jobs`, `task`,
  `one_off_tasks`, `secured`, `authorize`, `service`, `cached`, `api_doc`,
  `oauth2_callback`, `paths`, `step_up`, `ws` (when `ws` feature enabled).
  **Note**: `#[model]` and `#[repository]` are NOT in the prelude — use
  `#[autumn_web::model]` and `#[autumn_web::repository]` (qualified paths).
- Rendering: `asset_url`, `Markup`, `PreEscaped`, `html!`.
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
