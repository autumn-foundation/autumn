# autumn-web API Reference (0.7.0)

Use this file as a quick map for public names, features, dependency versions,
and config keys. Verify against current source when exact code matters.

Version identity: this reference tracks **0.7.0**, the current release line,
which is also the version `trunk-dev` carries. Entries carry the release they
arrived in: **(0.6.0)** is absent from 0.5.x, and **(0.7.0)** is absent from
0.6.x and earlier. Unmarked entries predate 0.6.0.

## Published crates

| Crate | Directory | Notes |
|---|---|---|
| `autumn-macros` | `autumn-macros/` | Proc macros; publish first |
| `autumn-schema-core` | `autumn-schema-core/` | Schema primitives shared by the CLI; no Autumn runtime deps |
| `autumn-edge` | `autumn-edge/` | Edge/WASM capsule runtime; `autumn-web` pins it (optionally) so it publishes before `autumn-web` |
| `autumn-web` | `autumn/` | Main framework crate; import path `autumn_web` |
| `autumn-cli` | `autumn-cli/` | Binary crate; binary name `autumn` |
| `autumn-admin-plugin` | `autumn-admin-plugin/` | First-party admin UI plugin |
| `autumn-media-plugin` | `autumn-media-plugin/` | Live-streaming media plugin (broadcast + rooms) |
| `autumn-storage-s3` | `autumn-storage-s3/` | S3-compatible `BlobStore` plugin |
| `autumn-cache-redis` | `autumn-cache-redis/` | Redis cache plugin |
| `autumn-search` | `autumn-search/` | Keyword + vector search plugin |

All publishable crates share the `[workspace.package]` version and release
together at `0.7.0`. This table lists the same crates, in the same order, as
`CRATES` in `scripts/check-publish-dry-run.sh` — that script is the executable
copy of the publish order.

## Top-level exports

### Functions

- `autumn_web::app() -> AppBuilder`
- `autumn_web::slugify(&str) -> String` — URL-safe slug. **Never returns
  `""`**: input with nothing to slugify (empty, all punctuation, un-folded
  non-Latin) gets a stable, deterministic hash fallback token instead.
- `autumn_web::contains_letter_or_number(&str) -> bool` (unreleased, #2424) —
  the input check `slugify` cannot answer. Reach for it to reject content-free
  user input (`"***"`, `"🎉🔥💯"`); **never** `slugify(x).is_empty()`, which is
  always `false` and so is dead code. Deliberately broader than "`slugify`
  produced a real slug": `"日本語"` passes — real text, hashed URL segment.

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
- Resumable SSE **(0.6.0, issue #1356)**:
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
| `#[autumn_web::main]` | Tokio runtime + Autumn profile bootstrap; optional runtime args `flavor` (`"multi_thread"` default / `"current_thread"`), `worker_threads`, `max_blocking_threads`, `thread_name`, `thread_stack_size`, `thread_keep_alive = "30s"`, and `configure = path::to::fn` — a `fn(&mut tokio::runtime::Builder)` run last, the escape hatch for `Builder` methods the args don't name (unreleased). Numeric args take expressions, not only literals. No args = tokio defaults; an unknown/duplicate/zero arg, or `worker_threads` under `current_thread`, is a compile error |
| `#[static_get]`, `static_routes![...]` | Static pre-render routes for `autumn build`; also accepts `params`, `revalidate`, and `seo(...)`. The `Content-Type` the handler declares is recorded per route in `dist/manifest.json` and served verbatim (unreleased, #1832) — set it explicitly for non-HTML routes (`application/xml`, `application/rss+xml`) since the serve path no longer infers it from the route slug |
| `static_gen::ManifestEntry` | One `dist/manifest.json` route entry: `file`, `revalidate`, `content_type`. `#[non_exhaustive]` — build with `ManifestEntry::new(file).with_revalidate(..).with_content_type(..)` (unreleased, #1832) |
| `static_gen::StaticFileLayer::resolve_entry` → `ResolvedStatic` | Manifest lookup returning the file path **and** the ready-to-serve `Content-Type`; `resolve` is the file-path-only shorthand. `static_gen::resolved_content_type` is the decision function: recorded type → recognized route extension → served file name → `application/octet-stream` (unreleased, #1832) |
| `#[ws]` | WebSocket route handler (`ws`) |
| `#[model]` | Diesel model derives (`db`) |
| `#[repository]` | CRUD repository and generated API (`db`); `mcp` / `mcp = "read"` expose the generated routes as MCP tools; `invalidates(path::to::cached_fn)` declares a cache-coherence invalidation edge proven by `autumn cache audit` (#1716) |
| `#[service]` | Service implementation scaffolding (`db`) |
| `#[secured]` | Session auth and role guard |
| `#[public]` | Marks a route handler as deliberately unauthenticated for the `autumn routes audit` coverage manifest — mirrors `#[secured]`, classifying the route `public` vs `gated`/`framework`/`unclassified` (0.6.0, #1604) |
| `#[authorize]` | Record-level policy guard |
| `#[api_doc]` | Route OpenAPI metadata |
| `#[oauth2_callback]` | OAuth2/OIDC callback route |
| `#[cached]` | Memoize function results; `key(a, b)` narrows the cache key, `reads(Model, …)` declares the cache-coherence dependency set, `acknowledge_stale = "…"` opts out of the gate (#1716) |
| `#[scheduled]`, `tasks![...]` | Recurring scheduled tasks |
| `#[job]`, `jobs![...]` | Request-triggered background jobs |
| `#[task]`, `one_off_tasks![...]` | Operator tasks invoked by CLI |
| `paths![...]` | Typed route path helper module |
| `#[mailer]`, `#[mailer_preview]`, `mail_previews![...]` | Mail helpers (`mail`) |
| `t!(...)` | Compile-time checked translation lookup (`i18n`) |
| `#[feature_flag]` | Feature-flag definition |
| `#[inbound_mail]` | Inbound mail handler |
| `#[step_up]` | Step-up authentication guard |
| `#[throttle]` | Per-route rate limit — inline (`limit`/`per`/`key`) or named (`#[throttle("login")]`) (**0.6.0**) |
| `#[event]`, `#[listener]`, `listeners![...]` | Typed domain event bus (**0.6.0**) — publish via the `Events` extractor, register with `.listeners(...)` |
| `#[query_budget(N)]` | Compile-time per-route database query ceiling — the build fails when a reachable path can exceed `N` (trunk-dev, #1667). Escape hatches: `#[query_budget(unbounded, reason = "…")]`, and `#[query_cost(N)]` / `#[query_exempt(reason = "…")]` on a statement |
| `authority_grant! { pub Name { … } }`, `#[agent_operable(grant = Name)]` | Build-time agent authority envelope (trunk-dev, #1691). The grant declares `writes`, `unbounded_writes`, `tenant_scope: scoped \| cross_tenant \| none`, `outbound` (literal URL prefixes or `alias:<name>`), `webhooks`, `jobs`, `rate`, `spend`, and a required `reversibility: reversible \| compensable \| irreversible`; the attribute statically derives the handler's effect set and the build fails at the offending call when the grant does not cover it. `#[agent_effect(writes(Model), …, reason = "…")]` / `#[agent_effect(none, reason = "…")]` on a statement declares what the analysis cannot read — it never grants. Requires nothing at runtime; pairs with `#[api_doc(mcp)]` |

Route macros accept a `seo(...)` argument declaring per-page meta tag defaults
(0.7.0, #1182):
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

**`sitemap.xml` / `robots.txt`** (0.7.0): `[seo] base_url` in `autumn.toml`
mounts `GET /robots.txt` and `GET /sitemap.xml`.
`[seo.robots]` takes `allow_all` (override the `dev`→`Disallow: /` /
`prod`→`Allow: /` profile default), `additional_rules`, and `sitemap_url`.
`AppBuilder::seo_source(source)` registers an
`autumn_web::seo::SitemapSource` supplying `SitemapEntry` values
(`loc` + optional `lastmod` / `changefreq` / `priority`); concrete
`#[static_get]` paths are derived automatically. `entries()` is awaited once at
router build, so the served body is a start-up snapshot — register your own
`/sitemap.xml` route for live entries (Autumn detects the collision, warns, and
mounts neither of its own SEO routes). `seo(robots = "noindex")` excludes a
route from the sitemap only for derived `#[static_get]` paths, never for
`SitemapSource` entries. Helpers: `seo::sitemap_xml` (truncates past 50,000
URLs), `seo::robots_txt`, `seo::write_seo_files` (used by `autumn build`).
Guide: `docs/guide/seo.md`.

**Locale-prefixed routing** (0.7.0, #1251): `[i18n]
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
struct-level attributes (0.6.0)
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
(0.6.0):

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
- **(0.7.0)** `#[translatable]` (issue #1384, needs the `i18n` feature) — the column stores
  an **independent value per locale tag** and resolves to the request's active
  locale with no locale argument in the handler. The field type becomes
  `autumn_web::i18n::Translated`, a per-locale container persisted as a JSON
  object in the field's own `TEXT` column (portable Postgres + `SQLite`);
  `Display` renders the active locale, so `html! { h1 { (post.title) } }` serves
  Spanish under `Accept-Language: es`. Resolution mirrors `Bundle`'s own
  algorithm — exact active locale, then each link of
  `I18nConfig::resolved_fallback_chain`, then one documented sentinel (`None`
  from `resolve()`, `""` from `Display`); never a panic or a 500, in or out of a
  request. An `AmbientLocaleLayer` (installed with the translation bundle, and
  inside each `/{locale}/…` nest) publishes the locale for the whole handler and
  wraps the response body, so a streaming/SSE render still resolves per frame;
  `i18n::with_locale(tag, fut)` scopes one explicitly for a job or mailer.
  Generated: per-field `<f>_localized()` / `<f>_in(locale)` /
  `set_<f>(locale, value)` / `<f>_locales()` / `<f>_is_translated(locale)`, plus
  field-name-keyed `available_locales(field)` / `is_translated(field, locale)` /
  `translated(field)` / `localized(field)` / `Model::translatable_fields()`, and
  an `i18n::TranslatableColumnDescriptor` inventory registration.
  **Write semantics matter**: the value *is* the whole map, so `find` →
  `set_title("es", …)` → `update` leaves every other locale intact, but
  *assigning* a `Translated` replaces the container — use
  `Translated::merge_from(&incoming)` for a partial update. `Serialize` is
  lossless (a map); `Deserialize` refuses a bare string, so
  `PUT {"title": "Hola"}` is a 422 rather than a silent wipe of every other
  language. Refused in combination with `#[encrypted]`, `#[searchable]`,
  `unique`/`indexed`, `#[normalize]`, `#[state_machine]`, `#[id]`,
  `#[lock_version]`, `#[position]`, `#[serde(rename)]` and
  `#[diesel(column_name)]`. `Option<Translated>` is a compile error — the empty
  container already means "no translation". A pre-existing plain-text column can
  be declared `#[translatable]` with **no data migration**; keys are never gated
  on locale-tag shape, so every key an app can write round-trips through the
  column.
- **(0.7.0)** `#[classified]` / `#[classified(personal_data)]` (issue #1654) — marks a
  non-null `String` column as **personal data** and carries that classification
  on the *type*, not in a name denylist. The generated field becomes
  `autumn_web::classify::Classified<String, {Model}{Column}Classified>` — a
  wrapper with no `Serialize`, `Display`, `Deref`, `Hash` or `into_inner` — and
  the model loses its `Serialize` derive as a consequence, so `Json(model)` and
  `Json(Dto { email: model.email })` are both **compile errors** naming the
  offending field and the `Json` sink. `Json`'s `IntoResponse` is bounded on
  `classify::JsonSink` (blanket over `Serialize`, `#[diagnostic::do_not_recommend]`),
  which is the seam later sinks plug into. Release is declared, never incidental:
  `autumn_web::declassify! { pub NAME: {Model}{Column}Classified => JsonResponse,
  purpose = "…", reason = "…" }` yields a boundary typed to exactly one column,
  and `value.declassify(&NAME)` takes the value **by move** (a release is a
  single event) and emits an auditable `tracing` record on
  `autumn::declassification` carrying model/field/tier/purpose/sink/reason —
  never the value. Purpose and reason must be non-blank literals. The write
  structs (`NewX`/`UpdateX`/`Changeset`) and the generated `XFactory` still
  accept the value — taking personal data *in* is not a release — but carry the
  wrapper too (the factory's setter still takes the plain type, so
  `.email("a@b.c")` is unchanged)
  (`Classified<String, F>`, `Patch<Classified<String, F>>` on the patch, so
  building one by hand costs an `.into()`) and get `#[serde(skip_serializing)]`;
  their fields are `pub`, so a bare `String` would have let a handler move the
  plaintext into a response view with no boundary;
  `Debug` renders `<classified>` on every generated struct; `#[validate]` still
  runs (the wrapper forwards `validator`'s string rules, and the two
  value-returning accessors `as_email_string`/`as_url_string` return `None` so
  they cannot hand the plaintext back). The column is also excluded from the
  client-controlled `list()` allowlists, so `?filter[col]=` and `?sort=col` can
  neither probe nor order by it. Refused in combination with `#[encrypted]`,
  `#[searchable]`, `#[normalize]`, `#[translatable]`, `#[id]`, `#[lock_version]`,
  `#[position]`, `#[state_machine]`, a `tenant_id` column, `#[serde(rename)]` /
  `rename_all`, and `#[serde(with/serialize_with/deserialize_with)]`; non-`String`
  fields are a compile error (mirrors `#[encrypted]`). A `#[repository]` that is
  `versioned`/`ledgered` does not compile against a classified model, and durable
  commit hooks refuse the payload at runtime — both snapshot the whole record.
  Generated: the field marker + `classify::ClassifiedField` impl (with a
  `module_path!()`-qualified `MODEL_PATH`), a
  `classify::manifest::ClassifiedFieldDescriptor` inventory registration, and
  `Model::__AUTUMN_CLASSIFIED_COLUMNS`. `autumn data-flow` emits the manifest.
  See `docs/guide/data-classification.md`.
- `#[normalize(trim, downcase, upcase, squish, strip_nul, with = path::to::fn)]` (issue
  #1379) — canonicalizes a `String` column, composing normalizers
  left-to-right. Built-ins live in `autumn_web::normalize`
  (`trim`/`downcase`/`upcase`/`squish`/`strip_nul`); `with = path` calls a user
  `fn(&str) -> String`. Runs on the **write** path (`save`, `save_many`,
  `save_many_skip_invalid` and the create half of `find_or_create_by_*` on
  insert; `update` via `UpdateDraft::from_patch`) *before* the model's
  `#[validate]` rules, the `before_create` / `before_update` hooks and the DB
  write, and on derived `#[repository]`
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

**(0.6.0)**: `preload(records, spec)` (declarative associations);
`from_shard(&ShardedDb)`; `with_pool_untracked` (new on
the 0.6.0 rename of `with_pool`; 0.5.x repositories had no pool constructor);
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

**(0.6.0)** — `find_or_create_by_<field>[_and_<field>...]`: declare
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

**(0.6.0)** — typed grouped aggregate queries (#1364): declare
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

**(0.7.0)** — `#[commentable]` polymorphic comment helpers (#1367):
declaring `#[commentable(by = User, author_name = username)]` on a `#[model]`
emits `Model::COMMENTABLE_TYPE`, `Model::commentable_spec()`, an `inventory`
registration, and a `{Model}Comments` trait blanket-implemented for that
model's repository (same `M2mConnSource<Model = M>` bound as the m2m/votable
helpers). Import the trait as `_`, then:

```rust
use crate::models::PostComments as _;
use autumn_web::commentable::{Comment, CommentNode};

async fn add_comment(&self, parent_id: i64, author_id: i64, body: &str,
                     reply_to: Option<i64>) -> AutumnResult<Comment>;
async fn comment_thread(&self, parent_id: i64) -> AutumnResult<Vec<CommentNode>>;
async fn delete_comment(&self, parent_id: i64, comment_id: i64) -> AutumnResult<usize>;
async fn recompute_comment_count(&self, parent_id: i64) -> AutumnResult<i64>;

pub struct Comment { pub id: i64, pub parent_id: Option<i64>, pub author_id: i64,
                     pub body: String, pub created_at: chrono::NaiveDateTime,
                     pub author_name: Option<String> }
pub struct CommentNode { pub comment: Comment, pub depth: usize,
                         pub replies: Vec<CommentNode> }
```

One `comments` table keyed on `(commentable_type, commentable_id)` serves every
commentable model; `parent_id` threads replies. `#[commentable]` must be
written **below** `#[model]`. The model's `#[id]` and counter fields must both
be `i64` — both are compile-checked. `add_comment` row-locks the parent (the
polymorphic column has no foreign key, so the probe is the referential check),
enforces `max_depth` (default 5) and same-record `reply_to`, and moves
`comment_count` with the #1325 counter-cache primitive in the same transaction.
`comment_thread` is one query at any depth; `delete_comment` cascades to the
descendant subtree and is idempotent, and takes `parent_id` so a comment id
alone is never authority over a comment on another record.
`recompute_comment_count` is the drift repair (`counter_cache_recompute` would
be WRONG here — it keys on the fk column alone, which is shared across models).
Like `react()`, all four take their own pooled connection — never hold a `Db`
extractor across one. `autumn_web::commentable::router(cfg)` serves
`GET`/`POST /{commentable_type}/{parent_id}` for every registered model from a
single mount; it authorizes the **tenant**, never the record, so an app with
private records must set `CommentsConfig::authorize(...)`. Build a host page's
own thread with `commentable::thread_dom_id`/`thread_action` so the router's
re-render lands on the same element. See `docs/guide/commentable.md`.

**(0.7.0)** — `#[votable]` reaction helpers (#1362): declaring
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

**(0.6.0)** — `ListQuery` extractor + `SortDir` (#1126): an `Infallible`
query extractor parsing `?sort=<col>`, `?dir=asc|desc`, and
`?filter[<col>]=<val>`. It **never rejects** — an empty or unknown `sort` falls
back to the model's default order, and an invalid `dir` falls back to `asc`
(`SortDir::{Asc, Desc}`). The `#[repository]`-generated `list(&ListQuery,
&PageRequest)` applies a typed per-column allowlist via Diesel `.into_boxed()`,
so only real columns can reach SQL (unknown sort/filter columns are ignored, not
injected).

**(0.7.0)** — `Query<T>` decodes sequences and nested structures (#1972):
the extractor no longer delegates to `serde_urlencoded`, so a query field does
**not** have to be a scalar. A query string of unique scalar keys behaves
exactly as before; on top of that it accepts

| Wire form | Decodes into |
| --- | --- |
| `?tags=a&tags=b` | `Vec<String>` (repeated key) |
| `?tags[]=a&tags[]=b` | `Vec<String>` (append form) |
| `?tags[0]=a&tags[2]=c` | `Vec<String>` (indexed; gaps compacted) |
| `?filter[status]=open` | a nested struct / map field |
| `?items[0][sku]=A-1` | `Vec<Item>` |

So prefer a real nested type over the comma-separated-string and
JSON-in-a-string workarounds. This is the same bracket syntax `ListQuery`'s
`?filter[<col>]=<val>` already used, generalized to arbitrary objects,
sequences and depths — and it applies to the **query string only**: `Form<T>`
still decodes bodies through `serde_urlencoded`.

Behaviour worth knowing when writing handlers:

- A duplicated key in a **single-valued** position is a 400 (`?q=a&q=b` against
  a `String`), matching what serde's derive did before. A sequence field takes
  every occurrence.
- `[` and `]` in a key are now **structure**, so a `Query<HashMap<String,
  String>>` that used to receive `?filter[a]=1` as the literal key
  `"filter[a]"` now sees a nested object — give such a field a nested type, or
  accept it as `serde_json::Value`.
- An unrecognised key that the target never reads stays ignorable even if its
  bracket syntax is malformed or contradictory.
- Decode errors name the failing field path (`filter.limit: invalid u32 value`)
  and never echo the submitted value.

MCP `tools/call` dispatch renders a tool's `query` object into this same wire
format, so a tool advertising a sequence or nested object round-trips into the
handler's typed struct. See `docs/guide/mcp.md` and the
`autumn_web::query_string` module docs.

## Optimistic concurrency (`#[lock_version]`)

Declare a non-nullable `i32`/`i64` field named `lock_version` on a `#[model]`
and mark it `#[lock_version]`. The column becomes database-managed: it is
excluded from `New{Model}`, carried on `Update{Model}` as the **expected**
version (a plain required field, not a `Patch<T>`, so JSON `PUT`/`PATCH`
clients must send it), and `#[repository]`'s update raises
`RepositoryError::Conflict` — mapped to HTTP 409 — when the stored version
moved on. The model also gains a derived `etag()`.

**(0.7.0, #1318)** `autumn generate model` / `generate
scaffold` wire this from the field name alone: the attribute, an
`INTEGER/BIGINT NOT NULL DEFAULT 0` column (the INSERT never names it), a
hidden `lock_version` input on the scaffolded edit form, an `update` handler
whose write is `WHERE lock_version = $expected` + `SET lock_version =
lock_version + 1` in one statement, and a **409 re-render** of that same form —
author's input intact, inline `role="alert"` banner, the row's *current*
version in the hidden field so a second Save applies their edit on top. A
missing row is still 404. `:states(...)` transitions bump the version too.
On HTML scaffolds, refused (not silently half-wired) with `--live`,
`--sharded`, a `slug` column, or an `Attachment` field; `--live-validation` is
supported and `--api` is exempt from those gates (no form to wire). Never
allowed as a model's only insertable column or marked `:unique`. The scaffolded **admin** update and the
delete actions remain last-write-wins.

## Db transactions

- `Db::tx(f)` — READ COMMITTED, one attempt (0.5.0).
- `Db::tx_with(opts: TxOptions, f) -> Result<T, AutumnError>`
  (**0.6.0**) — closure gets `&mut AsyncPgConnection`; auto-retries
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
- **(0.6.0)**: `checkbox_input`, `number_input`, `date_input`,
  `datetime_input`, `select_input`.
- **(0.6.0)**: `form_for(&changeset,
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

## Typed accessible primitives (`autumn_web::a11y`, feature `maud`, 0.6.0, #1706)

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

## View widgets and UI (all 0.6.0)

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
- `autumn_web::widgets::{CommentThread, CommentView, comment_thread}` (#1367) —
  the view half of `#[commentable]`. `CommentThread::new(dom_id, action)`, then
  `.csrf_token(s)` / `.csrf_field(s)`, `.return_to(path)` (where a **non-htmx**
  submit comes back to; the framework router honours only a relative,
  single-slash path, so it cannot become an open redirect), `.max_depth(n)`
  (pass the model's own, so the UI never offers a reply the write path would
  `422`), `.label(s)` / `.empty_text(s)` / `.submit_label(s)` /
  `.reply_label(s)` / `.placeholder(s)`, `.body_field(s)` / `.reply_field(s)`,
  `.hx_target(s)` (defaults `#{dom_id}`), and `.read_only(Option<String>)` for
  a signed-out visitor (thread renders, every form disappears, prompt shown).
  Render with `comment_thread(&cfg, &CommentView::from_thread(&nodes)) ->
  Markup`; `CommentView` is a plain view struct so the widget compiles without
  `db` and can be built from any source. Nested `<ol>`s (depth is exposed to
  assistive technology, not just indented), one `<details>`-disclosed inline
  reply form per node, each an ordinary `<form method="post">` carrying
  `hx-post` / `hx-target` / `hx-swap="outerHTML"` — so it works with scripting
  off and swaps in place when htmx is present. Bodies are escaped and split on
  blank lines into `<p>`s; never HTML. `dom_id` is interpolated into the
  default `hx-target` selector and into each node's id, so build it yourself.
  Every form shares one `hx-sync="#{dom_id}:replace"` scope so two quick replies
  cannot race, the list is `aria-live="polite"`, and each reply control is named
  for the comment it answers. CSS hooks `.autumn-comments` /
  `.autumn-comments-error` / `.autumn-comments-empty` /
  `.autumn-comments-prompt` / `.autumn-comment-list` / `.autumn-comment` /
  `.autumn-comment-meta` / `.autumn-comment-author` / `.autumn-comment-time` /
  `.autumn-comment-body` / `.autumn-comment-reply` /
  `.autumn-comment-reply-toggle` / `.autumn-comment-form` /
  `.autumn-comment-label` / `.autumn-comment-input` / `.autumn-comment-submit`.
- `autumn_web::widgets::{BulkActionsConfig, bulk_actions_form,
  bulk_select_checkbox, bulk_actions_toolbar}` (#1312) — the no-JavaScript
  bulk-select + delete-selected flow. `BulkActionsConfig::new(action_url)`
  then `.field_name(s)` (default `"ids"`), `.submit_label(s)` (default
  `"Delete selected"`), `.select_label(s)` (the `aria-label` prefix, default
  `"Select row"`). Render with
  `bulk_actions_form(&cfg, csrf_token, csrf_field, submit_token,
  submit_field, content) -> Markup` — a plain `<form method="post"
  action=..>` holding the hidden submit-token and CSRF inputs, your
  `content`, then the `bulk_actions_toolbar` submit button — and put one
  `bulk_select_checkbox(row.id, &cfg)` in each row's first cell (typically
  `columns.insert(0, Column::new("", ..))` ahead of a `data_table`). Checked
  rows submit as repeated `ids=<id>` pairs; the server reads them with any
  repeated-key form parser. Keep non-selection page furniture (a "New …"
  link, a search box) outside the form — anything inside is submitted with
  the selection. Pass the submit-token pair (`SubmitToken` /
  `SubmitFormField`, same extractors the generated create/update forms take):
  `SubmitTokenLayer` waves a tokenless request straight through, so omitting
  it gives up double-submit protection on a destructive endpoint. Both hidden
  fields lead the form body because the layer only scans its first chunk, and
  a long selection would otherwise push the token past the scan cap. The
  toolbar emits no confirmation prompt: an inline `onclick="return
  confirm(..)"` is blocked by the default `script-src 'self'` CSP (the form
  would submit with no prompt), and `confirm_action` — the framework's
  server-rendered `window.confirm()` replacement — posts its own
  single-action form, so it cannot carry the checkbox selection. Confirm a
  batch with an interstitial page that lists the rows and asks for a second
  submit.
  CSS hooks `.autumn-bulk-form` / `.autumn-bulk-actions` /
  `.autumn-bulk-select`. `autumn generate scaffold` emits the whole wiring —
  checkbox column, form, `POST /{plural}/bulk_delete` handler — for standard
  HTML scaffolds.
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

## Cache read-through (0.6.0)

`autumn_web::cache::{get_or_compute, get_or_compute_with,
GetOrComputeOptions, CacheFillError, jittered_ttl}` — single-flight fills,
optional `.distributed_fill_lock(true)` / `.stale_while_revalidate(grace)`.

## Downloads (0.6.0)

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

### Range / 206 Partial Content (0.6.0)

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

## PDF generation (0.7.0)

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
- **(0.6.0)**: `queue = "name"` + `[jobs] queues` strict-priority list or
  `[jobs.queues]` weight table; tracked jobs (`job::enqueue_tracked`,
  `enqueue_tracked_for`, `TrackedJobHandle`, optional third `JobContext`
  handler arg, `GET /_autumn/jobs/{token}`, `jobs.tracking.*` config).

## Distributed locks (0.6.0)

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

## Embedded clustering (0.7.0, #1762)

- `autumn_web::cluster` — zero-dependency two-node clustering: authenticated
  TCP gossip membership plus one shared primitive, an eventually consistent
  cluster-wide grow-only counter. Always compiled (no cargo feature); inert
  unless `[cluster] enabled = true`. Not leader election, not mutual
  exclusion, not durable — for run-once work use `Lock` above.
- Handle: `state.extension::<ClusterHandle>() -> Option<Arc<ClusterHandle>>`
  (`None` when disabled). `ClusterHandle::{node_id() -> &str, local_addr() ->
  SocketAddr, members() -> Vec<ClusterMemberInfo>, counter(name) ->
  ClusterCounter}`; `ClusterMemberInfo { id, addr, status: Alive|Suspect,
  incarnation }` is the **local** view including this node.
- Counter: `ClusterCounter::increment()` / `increment_by(n)` (sync, local,
  never fails; replicated on the next push) and `get() -> u64` (saturating
  sum of every merged cell; may jump upward on a peer merge, never moves
  down — a lower bound, not an enforceable limit). Cells are keyed by
  `(node id, boot incarnation)`; counters live for the process lifetime only.
- Surfacing: `cluster:membership` health indicator (HealthOnly — one member is
  `UP`; details need `health.detailed = true`) and `autumn_cluster_*` metric
  families (members gauge, pushes sent/received, merges, frames
  rejected-by-reason, frames dropped).
- Guide: [docs/guide/clustering.md](../../../docs/guide/clustering.md) — wire
  format, failure semantics (eventually consistent; authenticated HMAC-SHA256,
  unencrypted — trusted networks only), and a two-terminal walkthrough.

## Auth additions

- Published 0.5.0: `autumn generate auth` session management (`{user}_sessions`
  table, `sessions()` / `revoke_session` / `revoke_other_sessions` /
  `revoke_all_sessions`, `/account/sessions` page, `[auth.sessions]` config).
- **(0.6.0)**: scoped service tokens — `IssueTokenSpec`,
  `issue_scoped_api_token`, `#[secured(scopes = [...])]`,
  `PolicyContext::has_scope/has_any_scope/has_all_scopes`, `autumn token
  issue --name/--scope/--expires-at | list | rotate`, admin `TokenAdminModel`.
- **(unreleased, #1394)**: admin impersonation —
  `autumn_web::auth::impersonation::{begin_impersonation, end_impersonation,
  impersonator_id, is_impersonating, impersonation_state, audit_actor_id, clear,
  Impersonation, ImpersonationGate, ImpersonationPolicy, ImpersonationTarget,
  ImpersonationState, IMPERSONATOR_SESSION_KEY, IMPERSONATED_SESSION_KEY}`, plus
  `AppBuilder::impersonation_gate`. Default-deny behind an
  `ImpersonationGate` registered in `AppState`, and refused outright without an
  audit sink
  (`allow_roles([..])` / `custom(policy)` / `deny_all()`); the session's
  effective user becomes the target while `Current::actor` — and therefore
  `#[repository(versioned)]` rows and audit events — stays the real
  impersonator. Both edges rotate the session id and emit
  `auth.impersonation.begin` / `.end` audit events carrying
  `actor_id` = the impersonator and `target_resource_id` = the target. No
  nesting (`409`); the impersonated role comes only from
  `ImpersonationPolicy::target_role`, never request input; the operator's
  step-up claim is stashed for the duration; a record whose recorded target no
  longer matches the session's effective user is stale and is ignored; an
  `auth.session_key` colliding with `RESERVED_SESSION_KEYS` is refused
  (`is_reserved_session_key`).
  Admin UI: `AdminPlugin::with_impersonation(gate)`,
  `autumn_admin_plugin::{impersonation_banner_for, impersonation_banner,
  ImpersonationBanner, AdminImpersonation, IMPERSONATION_BANNER_CSS}`, routes
  `POST {prefix}/impersonate` (gated) and `POST {prefix}/impersonate/stop`
  (ungated on purpose). Session-based auth only.

## Submit tokens (0.6.0, #1360)

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
  `oauth2_callback`, `paths`, `step_up`, `query_budget`, `agent_operable`,
  `authority_grant` (#1691), `ws` (when `ws`
  feature enabled).
  **Note**: `#[model]` and `#[repository]` are NOT in the prelude — use
  `#[autumn_web::model]` and `#[autumn_web::repository]` (qualified paths).
- Rendering: `asset_url`, `Markup`, `PreEscaped`, `html!`.
- Accessibility primitives (`maud` feature, 0.6.0):
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
| `plugin(plugin)`, `plugins(tuple)` | Plugin install (`autumn plugin add <crate>` writes the call for first-party plugins, #1606) |
| `listeners(listeners![...])` | Event listeners (**0.6.0**) |
| `static_gate(layer)`, `has_static_gate::<L>()`, `get_static_gate_types()` | Static pre-render gating middleware (**0.6.0**) |
| `with_shard_router(router)` | Sharding router (**0.6.0**) |
| `run()` | Start server |

## Deterministic time and entropy (`autumn_web::time`, `autumn_web::entropy`)

Read time and mint ids through the injected seams, never `Utc::now()` /
`Instant::now()` / `Uuid::new_v4()` directly — that is what lets a `#[sim_test]`
replay a run byte-for-byte from its seed, and what makes elapsed-time math immune
to a wall-clock jump in production.

| API | Purpose |
|---|---|
| `Clock` extractor -> `.now()` | Wall-clock instant, snapshotted at request start |
| `Clock` extractor -> `.monotonic()` | Monotonic *request-start* instant (**0.7.0**) |
| `AppState::monotonic()` | Live monotonic reading — the closing half of an elapsed measurement (**0.7.0**) |
| `MonotonicInstant::saturating_duration_since(earlier)` | Elapsed duration; never negative, never panics (**0.7.0**) |
| `MonotonicInstant::saturating_add(dur)` | Deadline arithmetic without `Instant + Duration`'s panic (**0.7.0**) |
| `time::monotonic_now()` | Real monotonic clock, for code with no `ClockSource` in scope (**0.7.0**) |
| `time::clock_unix_secs(clock)` / `clock_unix_duration(clock)` | Unix time from the injected clock |
| `ClockSource::now` / `ClockSource::monotonic` | The trait; `monotonic` is defaulted to real time, so a **virtual** clock must override it (**0.7.0**) |
| `Rng` extractor -> `.uuid_v4()` / `.uuid_v7(ms)` / `.next_u64()` | Ids and randomness from the injected `Entropy` source |
| `AppState::entropy()` | The same source, for framework/job code |
| `SystemClock` / `FixedClock` / `TickingClock` | Real, pinned, and steppable `ClockSource` implementations |

`tokio::time::pause()` virtualizes `tokio::time::Instant`, **not**
`std::time::Instant` — a raw `std::time::Instant` reads the real machine clock
even inside a `#[sim_test]`. For a deadline whose counterparty is
`tokio::time::sleep`, use `tokio::time::Instant`.

## Authored fault scenarios (`autumn_web::sim::FaultPlan`, #1680)

The authored lane beside the probabilistic `sim::Chaos` builder: name the exact
effect that must fail, attach the plan to a `TestApp`, and assert on (or commit)
the serializable record of what happened. Test-only; DB checkout and job
execution only (SMTP faults stay on `Chaos::smtp_faults`).

| API | Purpose |
|---|---|
| `FaultPlan::from_seed(u64)` | Start a plan; the seed drives only the `random_*` builders (and, by default, the app's entropy) |
| `.fail_db_checkout(n)` / `.fail_db_checkout_on("replica", n)` | Fail the n-th checkout (1-based) on the global / per-pool counter (`db` feature) |
| `.fail_job_execution(n)` / `.fail_job("send_invoice", n)` | Fail the n-th job execution on the global / per-name counter; the runtime's retry policy applies |
| `.random_db_checkout_faults(count, 1..=k)` / `.random_job_execution_faults(count, 1..=k)` | Seed-derived distinct ordinals, resolved into explicit entries at builder time |
| `.only_between(from, to)` | Half-open elapsed window on the app's **injected** clock; a match outside it is `suppressed`, not fired |
| `.planned()` / `.describe()` / `.is_active()` | The full authored schedule (sorted), a printable rendering, whether anything is planned |
| `TestApp::with_fault_plan(plan)` | Attach; composes with `with_job_interceptor` / `with_db_interceptor`, transactional isolation and `Sim::chaos` (fault innermost). Asserts `jobs.workers == 1`, `reporting.sample_rate == 1.0` and `failure_capture.enabled == false`; defaults entropy to `SeededEntropy(seed)` |
| `TestClient::fault_outcome().await` | Settle the detached error-report tasks (bounded yields, no clock advance), then snapshot a `FaultOutcome` |
| `TestClient::fault_ledger()` | The live handle (`Option`); `.outcome()` snapshots without settling |
| `FaultOutcome { seed, fired, suppressed, unfired, server_errors, final_state }` | `Serialize + Deserialize + Eq`; `to_json_string()` is canonical, `fingerprint()` is FNV-1a 64, `from_json_str` parses a committed record |

Ordinals are exact by construction; *which* pass is the n-th replays only under a
paused current-thread runtime (`#[sim_test]`) with jobs drained by
`Sim::run_to_idle` — `perform_enqueued_jobs` bypasses `intercept_execute`.

## SystemTest builder (`autumn_web::system_test`, feature `system-tests`)

Browser-driven tests: boots the app on an ephemeral port, launches managed
headless Chromium, returns a `Page` with htmx-aware auto-waiting assertions.
Dev-dependency only.

| Method | Notes |
|---|---|
| `SystemTest::new()` | Builder; `test` profile with CSRF disabled |
| `.routes(routes![...])` | Routes to serve; additive across calls |
| `.state(AppState)` | Supply a pre-built state (real DB pool, policies); its embedded config wins |
| `.layer(layer)` | App-wide Tower middleware, same `IntoAppLayer` bound and stack position as `AppBuilder::layer` — first call is outermost on ingress (**0.7.0**) |
| `.artifact_dir(path)` | Where failure screenshots/HTML go (default `target/system-tests/<test>/`) |
| `.browser_timeout(d)` / `.hx_settle_timeout(d)` | Launch and htmx-settle deadlines |
| `.build()` | Boot server + browser → `SystemTestRunner` |
| `SystemTest::attach(base_url)` | Browser only, against an already-running app (`attach_with_timeout` for a custom deadline) |
| `runner.page()` / `runner.base_url()` | Open a `Page`; the app's base URL |
| `BrowserCheck::run()` | Probe the host for Chromium; also what `autumn doctor` reports |

Reach for `.layer(...)` whenever the routes under test depend on global
middleware (tenant scoping, an auth shim, request enrichment) — mapping the
layer onto individual handlers instead tests a stack the real app never
serves.

`Page`: `visit`, `fill`, `click`, `expect_text`, `expect_url`,
`expect_attribute`, `expect_hx_settle`, `expect_sse_event`,
`expect_no_console_errors`, `console_errors`, `snapshot`, `evaluate`. The
`expect_*` assertions poll to a deadline and ignore the transient CDP errors a
mid-poll navigation produces; `evaluate` is a single raw call that does not.

See `docs/guide/system-tests.md`.

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
redis = ["dep:redis", "dep:rustls"]
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

`storage-s3` is not an `autumn-web` feature. Use `autumn-storage-s3 = "0.7"`.

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
redis = { version = "1.2.0", default-features = false, features = ["aio", "tokio-comp", "connection-manager", "script", "tokio-rustls-comp"] }
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

Declarative per-handler `Cache-Control` header (0.6.0, issue #1344).
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

Reading the resolved config from handler/service code (0.7.0, #2198): `AppState::config_arc() -> Arc<AutumnConfig>` is the cheap
accessor — a refcount bump, no deep clone; reach for it on anything that runs
per request. `AppState::config() -> AutumnConfig` is unchanged and still right
when you need an owned, independently mutable snapshot — it deep-clones every
section on each call.

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

`.env` auto-loading (0.6.0): a project-root `.env` file is a
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
| `AUTUMN_CHANNELS__REPLAY_BUFFER` | `channels.replay_buffer` (0.6.0) |
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
| `AUTUMN_OBSERVABILITY__SERVER_TIMING` | `observability.server_timing` (0.6.0) — bool; `Server-Timing` response header opt-in. Defaults on in `dev`/`development`, off elsewhere. See `docs/guide/observability/server-timing.md`. |

### `[cluster]` — embedded clustering (0.7.0, #1762)

Opt-in, zero-dependency two-node clustering (see "Embedded clustering" above).
Off by default; when enabled a secret of ≥16 bytes is required (fail-fast
validation, no unauthenticated mode).

```toml
[cluster]
enabled = true
secret = "…"                      # required, ≥16 bytes; prefer AUTUMN_CLUSTER__SECRET
cluster_name = "autumn"           # MAC-covered; two clusters cannot mix
bind_addr = "127.0.0.1:0"         # port 0 = ephemeral (resolved port is advertised)
advertise_addr = "10.0.1.7:7946"  # required for wildcard binds; port 0 rejected
seed_peers = ["10.0.1.8:7946"]    # dial list; CSV in env form; port 0 rejected
node_id = "web-1"                 # default: per-boot random id; no '#' allowed
push_interval_ms = 500            # ±20% per-node jitter
suspicion_timeout_ms = 2500       # ≥ 3× push_interval_ms enforced
```

Env form `AUTUMN_CLUSTER__<KEY>`; addresses are IP literals (no hostname
resolution).

### `[failure_capture]` — failure capsules (0.7.0, #1598)

Opt-in deterministic replay capsules: every caught handler panic or 5xx writes
one redacted JSON file (the request, the PostgreSQL wire traffic the handler
produced, its clock readings, and the outcome), replayable offline with
`autumn replay <capsule>`. Off by default; nothing is installed until enabled.

```toml
[failure_capture]
enabled = true            # default: false — arms capture layer, recording pool, recording clock
dir = "tmp/autumn-capsules"
max_body_bytes = 65536    # request-body copy cap (64 KiB)
max_capsule_bytes = 1048576
max_capsules = 50         # oldest-first prune (capsule-named files only), before each write
```

- `autumn replay <file>` exit codes: `0` reproduced (status + message +
  problem type match, tape fully consumed), `1` mismatch/diverged, `2`
  refused (truncated capsule, unknown `format_version`, sqlite build).
  Replay is offline by design: in-memory sessions, in-process channels,
  fail-closed outbound HTTP, no port bound, DB served from the capsule's tape
  by an in-process stub server. App-registered state initializers still run
  as written (documented boundary — point them at stubs when replaying).
- `autumn replay` compiles the replay binary with the build kind the capsule
  recorded (`app.debug_assertions`) so `cfg(debug_assertions)` code paths
  match the failing run; override with `--release`/`--debug`, and pass
  `--features`/`--no-default-features` when feature-gated code matters (the
  feature set itself is not recorded).
- A failing response with a *streaming* body (SSE, `Body::from_stream`) marks
  its capsule truncated: effects produced while the body streams are not on
  the tape.
- `ErrorEvent::capsule: Option<CapsuleRef>` — the file is on disk (and pinned
  against pruning) before reporters run.
- DB capture needs plaintext-TCP PostgreSQL: TLS-required URLs, Unix sockets,
  sqlite builds, custom `DatabasePoolProvider`s and shard pools disable the
  tape and mark capsules truncated (with a note saying why).
- **A capsule is production data** — result rows, path segments, and SQL text
  are not maskable; treat the directory like a database dump and read
  `docs/guide/failure-capsules.md` (security section leads) before enabling.

### `[server.tls]` (feature `tls`, 0.6.0, #1603)

In-process HTTPS termination on the same host:port (off by default).

- `cert_path`, `key_path` — PEM cert/key.
- `reload_interval_secs` (default `60`) — certs hot-reload by polling file
  mtimes.
- `handshake_timeout_secs` (default `10`).
- Fail-fast at startup on bad / missing / mismatched / expired PEM, on
  `[server.tls]` without the `tls` feature compiled in, and on `[server.tls]`
  combined with `server.unix_socket`.
- Everything else behaves as it does over plain HTTP: the `/health`, `/live`,
  `/ready`, `/startup` probes and `/actuator/health`, the `[server.timeouts]`
  request deadline, SSE streaming, `wss://` WebSockets, and graceful shutdown.
- `autumn_web::tls::CertReloader` is the public reload task (mtime polling)
  the app spawns; `CertReloader::load` builds the resolver and its reloader
  together so a renewal during startup cannot be missed.
- **In a container:** the image builder runs a bare `cargo build --release`, so
  `tls` must be a *default* feature of the app; mount the PEMs and set
  `AUTUMN_SERVER__TLS__CERT_PATH` / `__KEY_PATH`; set
  `AUTUMN_HEALTHCHECK_URL=https://localhost:3000/health` plus
  `AUTUMN_HEALTHCHECK_INSECURE=1` so the generated Dockerfile's HEALTHCHECK
  probes its own loopback listener over TLS instead of failing forever. See
  `docs/guide/tls.md`.

### `[server.tls.acme]` (feature `acme`, 0.6.0, #1608)

Automatic ACME certificate provisioning + renewal; builds on `tls`, off by
default. Mutually exclusive with static `cert_path` / `key_path`.

- `domains` (required), `contact_email` (required). Each domain is used verbatim
  as the certificate's SAN and the ACME order's DNS identifier, so an entry with
  leading/trailing whitespace is rejected at startup (#1874). A **wildcard**
  (`*.myapp.com`) is accepted only when `[server.tls.acme.dns]` is configured —
  no CA validates a wildcard identifier over HTTP-01 (#1620).
- `directory` — Let's Encrypt staging by default; `production` or a custom URL.
- `cache_dir` (default `config/acme`).
- `http_challenge_port` (default `80`).
- `renew_before_days` (default `30`, an unquoted whole number, must be `< 90`).
- `ca_root_path` (unset) — PEM root that signs the ACME **directory's own HTTPS
  certificate**. Needed only for a private CA / Pebble reached through a custom
  `directory`: by default the client verifies the directory against the platform
  trust store, which is correct for Let's Encrypt. It REPLACES the trust anchors
  and only the file's **first** certificate is installed, so pass the root
  alone, not a bundle. Affects the ACME control plane only, never what browsers
  accept from the site.
- Automatic HTTP-01 provisioning + hourly leader-elected renewal.

### `[server.tls.acme.dns]` (feature `acme`, unreleased — trunk-dev, #1620)

Answers every authorization over **DNS-01** instead of HTTP-01, which is what a
**wildcard** certificate requires — so one `*.myapp.com` covers every tenant
subdomain, and onboarding tenant N+1 costs no certificate work at all. Renewal,
persistence, staging selection and health are #1608's, unchanged.

```toml
[server.tls.acme]
domains = ["myapp.com", "*.myapp.com"]
contact_email = "ops@myapp.com"

[server.tls.acme.dns]
provider = "cloudflare"
```

- `provider` (required) — `cloudflare`, `route53`, or `exec`.
- `credential` (default `acme_dns`) — the key in the **encrypted credentials
  store** holding the provider's API credential. A key NAME, never a token: the
  section has no field that could hold one and is `deny_unknown_fields`, so an
  `api_token = "..."` in `autumn.toml` is a startup error. Values come from
  `autumn credentials edit` or the `AUTUMN_ACME_DNS_*` environment variables
  (`API_TOKEN`, `ACCESS_KEY_ID`, `SECRET_ACCESS_KEY`, `SESSION_TOKEN`,
  `HOSTED_ZONE_ID`, `REGION`), which override the store field for field.
- `propagation_timeout_secs` (default `300`, max `3600`) — bound on the wait for
  the TXT record to become visible. The timeout error names the exact record,
  the value and the server that never saw it.
- `poll_interval_secs` (default `5`), `resolvers` (default Cloudflare + Google
  public DNS; each entry an IP or `IP:port`, hostnames rejected). The resolvers
  DISCOVER the zone's authoritative nameservers; the propagation probe goes to
  those, because a recursive resolver caches a negative answer for longer than
  the budget. Discovery runs per distinct challenge name, so a multi-domain
  order probes each zone through its own servers. Authoritative probes are sent
  with recursion NOT desired; the fallback to the configured resolvers sets it,
  since a recursive resolver asked without it answers from cache only.
- `command` — the `exec` hook's argv array, run without a shell as
  `hook present|cleanup <fqdn> <value>` (exactly three appended arguments, no
  `--` marker, so `$1` is the action). Required for `exec`, rejected for the
  others. This is the escape hatch for any provider not listed (RFC 2136 via
  `nsupdate`, a registrar CLI, a webhook shim). The hook's `stderr` is read
  through a bounded buffer and scrubbed of credential-shaped environment values
  before it reaches a log, an alert, or `/actuator/health`.
- Under DNS-01 the CA never connects to this host, so a failure to bind
  `http_challenge_port` is a warning rather than a fatal error — the listener is
  then only the HTTP→HTTPS redirect.
- A failed issuance/renewal raises #1610's `scheduled_task_failure` operator
  alert for `acme-renewal` and clears it on the next success; the `acme` health
  indicator reports `challenge` and `dns_provider`.

See `docs/guide/tls.md`.

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
