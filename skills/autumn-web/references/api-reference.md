# autumn-web 0.5.0 API Reference

Use this file as a quick map for public names, features, dependency versions,
and config keys. Source of truth is the workspace at version 0.5.0; verify
against current source when exact code matters.

## Published crates

| Crate | Directory | Notes |
|---|---|---|
| `autumn-macros` | `autumn-macros/` | Proc macros; publish first |
| `autumn-web` | `autumn/` | Main framework crate; import path `autumn_web` |
| `autumn-cli` | `autumn-cli/` | Binary crate; binary name `autumn` |
| `autumn-admin-plugin` | `autumn-admin-plugin/` | First-party admin UI plugin |
| `autumn-storage-s3` | `autumn-storage-s3/` | S3-compatible `BlobStore` plugin |
| `autumn-cache-redis` | `autumn-cache-redis/` | Redis cache plugin |

All publishable crates share `[workspace.package].version = "0.5.0"`.

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

- `Mail`, `Mailer`, `MailConfig`, `MailTransport`, `MailDeliveryQueue`,
  `MailDeliveryQueueHandle`, `Transport`, `SmtpConfig`,
  `TlsMode` (`mail`) — `MailPreview` is available via `autumn_web::mail::MailPreview`
  (not re-exported at the crate root)
- `DbApiTokenStore`, `API_TOKEN_MIGRATIONS`, repository hooks (`db`)
- `Multipart` (`multipart`)
- `Flash`, `FlashLevel`, `FlashMessage` (`flash`)
- `Broadcast`, `Channels`, `ChannelsBackend`, `LocalChannelsBackend`,
  `ChannelMessage`, `ChannelStats` (`ws`)
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
| `#[repository]` | CRUD repository and generated API (`db`) |
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

`#[model]` also recognizes `#[belongs_to]` / `#[has_many]` / `#[has_one]`
struct-level attributes for declarative associations with batched eager
preloading (`Model::preload()`, `repo.preload(records, spec)`); these are
consumed by `#[model]` itself, not separately-registered proc macros.
`#[has_many(Target, through = join_table)]` declares a many-to-many
association through a join table, adding `add_{singular}` /
`remove_{singular}` / `set_{plural}` mutation helpers to the generated
`#[repository]`. See the `#[model]` doc comment in `autumn-macros/src/lib.rs`
and `docs/adr/0008-associations-and-eager-loading.md`.

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
| `mount_mcp(path)`, `expose_all_as_mcp()`, `secure_mcp(layer)` | MCP endpoint projection (`mcp`) |
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
