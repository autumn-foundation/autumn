# autumn-web Example Reference (0.7.0)

Use these patterns when generating or reviewing Autumn apps. The official
examples live under `examples/`; prefer current source when exact code matters.
Everything here works on the published 0.7.0 crates unless marked otherwise.

## Status field with a state machine (replaces hand-rolled hook validation)

Do NOT write status-transition `match` blocks in `before_create`/
`before_update` hooks or handlers — declare the graph on the model instead:

```rust
#[autumn_web::model]
pub struct Page {
    #[id]
    pub id: i64,
    pub title: String,
    pub body: String,
    #[state_machine(transitions(
        draft -> published: "can_publish",
        published -> archived,
    ))]
    pub status: String,
}

impl Page {
    fn can_publish(&self) -> bool {
        !self.title.is_empty() && !self.body.is_empty()
    }
}

// In PageHooks::before_update — the only transition code you write:
async fn before_update(
    &self,
    _ctx: &mut MutationContext,
    draft: &mut UpdateDraft<Page>,
) -> AutumnResult<()> {
    if draft.after.status != draft.before.status {
        let mut proposed = draft.after.clone();
        proposed.status = draft.before.status.clone();
        proposed.transition_status_to(&draft.after.status)?; // 400 on bad edge/guard
    }
    Ok(())
}
```

See `examples/wiki/src/models.rs` + `examples/wiki/src/hooks.rs` and
`docs/guide/state-machines.md`.

## Minimal app

```rust
use autumn_web::prelude::*;

#[get("/")]
async fn index() -> &'static str {
    "Welcome to Autumn!"
}

#[get("/hello/{name}")]
async fn hello_name(Path(name): Path<String>) -> String {
    format!("Hello, {name}!")
}

#[autumn_web::main]
async fn main() {
    autumn_web::app()
        .routes(routes![index, hello_name])
        .run()
        .await;
}
```

Published-user dependency:

```toml
[dependencies]
autumn-web = "0.7"
```

Workspace examples use `autumn-web = { path = "../../autumn" }` plus the root
`[patch.crates-io] autumn-web = { path = "autumn" }`.

## Blog - static pre-rendering, CRUD, admin routes

Pattern from `examples/blog/src/main.rs`:

```rust
mod models;
mod routes;
mod schema;

use autumn_web::migrate::{embed_migrations, EmbeddedMigrations};
use autumn_web::{routes, static_routes};

const MIGRATIONS: EmbeddedMigrations = embed_migrations!();

#[autumn_web::main]
async fn main() {
    autumn_web::app()
        .migrations(MIGRATIONS)
        .routes(routes![
            routes::about::about,
            routes::posts::index,
            routes::posts::show,
            routes::posts::admin_list,
            routes::posts::new_form,
            routes::posts::create,
            routes::posts::edit_form,
            routes::posts::update,
            routes::posts::delete_post,
            routes::api::list_json,
            routes::api::create_json,
        ])
        .static_routes(static_routes![routes::about::about])
        .run()
        .await;
}
```

Takeaways:

- `#[static_get]` routes still belong in `.routes(...)` for runtime serving.
- Add the same handler to `.static_routes(...)` for `autumn build`.
- CRUD HTML and JSON routes can coexist without a SPA boundary.

## Reddit clone - comprehensive app pattern

`examples/reddit-clone` is the broadest reference. It demonstrates auth,
sessions, CSRF, `#[secured]`, `#[model]`, `#[repository]`, mutation hooks,
`#[scheduled]`, `#[static_get]`, `#[ws]`, `#[job]`, plugins, mail, storage,
Redis, and htmx voting.

```rust
use autumn_web::migrate::{embed_migrations, EmbeddedMigrations};
use autumn_web::prelude::*;
use reddit_clone::{live_events, repositories, routes, tasks};

const MIGRATIONS: EmbeddedMigrations = embed_migrations!();

#[autumn_web::main]
async fn main() {
    autumn_web::app()
        .migrations(MIGRATIONS)
        .routes(routes![
            routes::posts::front_page,
            routes::about::about,
            routes::auth::register_form,
            routes::auth::register,
            routes::auth::login_form,
            routes::auth::login,
            routes::auth::logout,
            routes::auth::profile,
            routes::subreddits::list,
            routes::subreddits::create_form,
            routes::subreddits::create,
            routes::subreddits::show,
            routes::posts::submit_form,
            routes::posts::submit_to_sub_form,
            routes::posts::submit,
            routes::posts::show,
            routes::posts::edit_form,
            routes::posts::update,
            routes::posts::delete_post,
            routes::comments::create,
            routes::comments::list_comments,
            routes::votes::upvote,
            routes::votes::downvote,
            routes::live::live_feed_health,
            routes::live::live_feed,
            routes::live::subreddit_feed,
            repositories::subreddit_api_list,
            repositories::subreddit_api_get,
            repositories::post_api_list,
            repositories::post_api_get,
        ])
        .static_routes(static_routes![routes::about::about])
        .tasks(tasks![
            tasks::recalculate_hot_ranks,
            tasks::prune_live_feed_events,
        ])
        .jobs(reddit_clone::jobs::registered_jobs())
        .plugin(live_events::LiveFeedPlugin::new())
        .run()
        .await;
}
```

Feature set:

```toml
autumn-web = { version = "0.7", features = ["mail", "ws", "storage", "multipart", "redis"] }
```

Keep Harvest out of core web examples. Use built-in jobs for app-local work and
recommend Autumn Harvest only for durable multi-step workflows.

## WebSocket, broadcast, and SSE

Pattern from `examples/ws-echo`:

```rust
use autumn_web::prelude::*;
use autumn_web::ws::{Message, WebSocket, WithShutdown, WsHandler};
// WithShutdown requires CancellationToken — tokio-util is a transitive dep,
// not a re-export, so add it to Cargo.toml: tokio-util = { version = "0.7", features = ["sync"] }
use tokio_util::sync::CancellationToken;

#[ws("/echo")]
async fn echo() -> impl WsHandler {
    |mut socket: WebSocket| async move {
        while let Some(Ok(msg)) = socket.recv().await {
            if let Message::Text(text) = msg
                && socket.send(Message::Text(text)).await.is_err()
            {
                break;
            }
        }
    }
}

#[ws("/chat")]
async fn chat(state: AppState) -> impl WsHandler {
    let channels = state.channels().clone();
    let tx = channels.sender("lobby");
    let mut rx = channels.subscribe("lobby");

    WithShutdown(
        |mut socket: WebSocket, shutdown: CancellationToken| async move {
            loop {
                tokio::select! {
                    incoming = socket.recv() => {
                        if let Some(Ok(Message::Text(text))) = incoming {
                            tx.send(text.to_string()).ok();
                        }
                    }
                    broadcast = rx.recv() => {
                        if let Ok(msg) = broadcast
                            && socket.send(Message::Text(msg.into_string().into())).await.is_err()
                        {
                            break;
                        }
                    }
                    () = shutdown.cancelled() => {
                        socket.send(Message::Close(None)).await.ok();
                        break;
                    }
                }
            }
        },
    )
}

#[get("/events")]
async fn events(State(state): State<AppState>) -> impl IntoResponse {
    autumn_web::sse::stream(&state, "lobby-html")
}
```

Use `state.broadcast().publish_html(channel, &markup)` for htmx-ready SSE
fragments. Use Redis channels for multi-replica deployments.

## Signed webhooks

Pattern from `examples/signed-webhooks/src/lib.rs`:

```rust
use autumn_web::prelude::*;
use autumn_web::webhook::{WebhookConfig, WebhookEndpointConfig, WebhookProvider};

#[post("/webhooks/stripe")]
async fn stripe(webhook: SignedWebhook) -> AutumnResult<Json<serde_json::Value>> {
    let payload = webhook.json::<serde_json::Value>().map_err(|error| {
        AutumnError::bad_request_msg(format!("invalid webhook JSON payload: {error}"))
    })?;

    Ok(Json(serde_json::json!({
        "accepted": true,
        "provider": webhook.provider(),
        "delivery_id": webhook.delivery_id(),
        "event_type": webhook.event_type(),
        "payload": payload,
    })))
}

pub fn routes() -> Vec<autumn_web::Route> {
    routes![stripe]
}

pub fn config() -> autumn_web::config::AutumnConfig {
    autumn_web::config::AutumnConfig {
        profile: Some("test".to_owned()),
        security: autumn_web::security::SecurityConfig {
            csrf: autumn_web::security::CsrfConfig {
                enabled: false,
                ..Default::default()
            },
            webhooks: WebhookConfig {
                endpoints: vec![
                    WebhookEndpointConfig::new(
                        "stripe",
                        "/webhooks/stripe",
                        WebhookProvider::Stripe,
                        "dev-stripe-webhook-secret-32-bytes",
                    )
                    .with_timestamp_tolerance_secs(300)
                    .with_replay_window_secs(86400),
                ],
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    }
}
```

Production config should use `secret_env` and Redis replay storage instead of
inline secrets and memory replay. See `docs/guide/signed-webhooks.md`.

## Distributed bookmarks - plugin and topology pattern

`examples/bookmarks-distributed` shows primary/replica pools, explicit
production database roles, Postgres-coordinated scheduled work, the Redis
cache plugin, and a two-node `[cluster]` between its web replicas:

```rust
use autumn_cache_redis::RedisCachePlugin;
use autumn_web::migrate::{embed_migrations, EmbeddedMigrations};
use autumn_web::prelude::*;

const MIGRATIONS: EmbeddedMigrations = embed_migrations!();

#[autumn_web::main]
async fn main() {
    autumn_web::app()
        .plugin(RedisCachePlugin::new())
        .migrations(MIGRATIONS)
        .routes(routes![
            routes::bookmarks::list,
            routes::bookmarks::by_tag,
            routes::bookmarks::new_form,
            routes::bookmarks::create,
            routes::cluster::status,
            repositories::bookmark_api_count,
            repositories::bookmark_api_list,
            repositories::bookmark_api_get,
            repositories::bookmark_api_create,
            repositories::bookmark_api_update,
            repositories::bookmark_api_delete,
        ])
        .tasks(tasks![tasks::check_links])
        .run()
        .await;
}
```

For new production config prefer:

```toml
[database]
primary_url = "postgres://..."
replica_url = "postgres://..."
replica_fallback = "fail_readiness"
auto_migrate_in_production = false

[scheduler]
backend = "postgres"
```

## Admin plugin

Install the first-party admin UI:

```toml
autumn-web = { version = "0.7", features = ["db", "flash", "htmx", "maud"] }
autumn-admin-plugin = "0.7"
```

```rust
use autumn_admin_plugin::{prelude::*, AdminPlugin};
use autumn_web::prelude::*;

#[autumn_web::main]
async fn main() {
    autumn_web::app()
        .plugin(AdminPlugin::new())
        .run()
        .await;
}
```

The plugin mounts at `/admin` by default and requires the `admin` session role.
It includes jobs, feature-flag, and experiment administration
surfaces.

## S3 storage plugin

```toml
autumn-web = { version = "0.7", features = ["storage", "multipart"] }
autumn-storage-s3 = "0.7"
```

```rust
use autumn_storage_s3::S3BlobStore;
use autumn_web::prelude::*;

#[autumn_web::main]
async fn main() {
    let config = autumn_web::config::AutumnConfig::load()
        .expect("config");
    let store = S3BlobStore::from_config(&config.storage.s3)
        .await
        .expect("S3 store");

    autumn_web::app()
        .with_blob_store(store)
        .run()
        .await;
}
```

`autumn-web` keeps the `BlobStore` trait and local backend. S3 lives in
`autumn-storage-s3`.

## Seekable video from a stored blob (Range / 206)

Serve a private stored video behind a policy so a browser `<video>` element can
seek. `into_response_ranged` reads the request's `Range` header and answers with
`206 Partial Content`, fetching only the requested byte slice from the store
(never buffering the whole object). A non-ranged request gets `200` with
`Accept-Ranges: bytes`; an unsatisfiable range gets `416`.

```rust
use autumn_web::download::Download;
use autumn_web::storage::SharedBlobStore;
use autumn_web::{secured, AutumnError};
use http::HeaderMap;

#[secured(policy = "media.watch")]
async fn watch(
    store: SharedBlobStore,
    key: String,
    headers: HeaderMap,
) -> Result<axum::response::Response, AutumnError> {
    Ok(Download::from_blob(&store, key)
        .await?
        .content_type("video/mp4")
        .inline()
        // `.etag(..)` / `.last_modified(..)` make If-Range meaningful so a
        // resumed download resyncs when the object changed underneath it.
        .into_response_ranged(&headers)
        .await)
}
```

Under the hood the response goes through `autumn_web::range` (single-range
parsing, multi-range single-range collapse, `If-Range`) and the blob slice is
read via `BlobStore::get_range` (the local backend seeks + takes off disk).

## Where the flagship 0.7.0 subsystems are exemplified

Each of these has a full guide and, as of #2320, a runnable example. Read the
example when you need working code rather than prose — the wiring details below
are the ones that are easy to get wrong.

| Subsystem | Guide | Working code |
| --- | --- | --- |
| Deterministic simulation testing | `docs/guide/simulation-testing.md` | `examples/reddit-clone/tests/sim_hot_rank.rs` |
| Failure capsules | `docs/guide/failure-capsules.md` | `examples/reddit-clone/autumn-capsules.toml`, `capsules/`, `tests/failure_capsule.rs` |
| Self-clustering substrate | `docs/guide/clustering.md` | `examples/bookmarks-distributed` (`autumn-docker.toml`, `docker-compose.yml`, `src/routes/cluster.rs`) |
| App metrics facade | `docs/guide/metrics.md` | `examples/bookmarks/src/metrics.rs` |

**`#[sim_test]`** — mount the app on the sim's paused runtime and read time
through the ordinary `Clock` extractor; a handler that computes its own `now`
is not being tested by the sim at all. `always!` for invariants (it panics and
prints the `AUTUMN_SIM_SEED=…` replay line), `sometimes!` for reachability. A
single run does **not** fail on an unsatisfied `sometimes!` — if you want that,
arrange the workload so every label is reachable at any seed and call
`assert_all_sometimes_satisfied()` explicitly. `Sim::build` injects the clock
but **not** entropy: pass `.with_entropy(SeededEntropy::new(sim.seed))` or a
later `Rng` draw silently stops replaying from the seed.

**Failure capsules** — put `[failure_capture] enabled = true` in its own
profile, not the dev one: a capsule is production data and database result rows
are raw wire bytes that are never masked. Redaction matches parameter names by
**equality after normalization**, never by prefix, so a prefixed secret header
(`x-api-key`, `stripe-signature`) is recorded verbatim unless the app names it
in `[log] filter_parameters`. A custom profile does not auto-apply migrations —
set `auto_migrate = true` if the profile is meant to behave like dev.

**`[cluster]`** — the settings both nodes share belong in the profile; the node
id, advertise address and seed peers differ per instance and belong in the
deployment (compose/env). A wildcard `bind_addr` **requires** an explicit
`advertise_addr`, and the config parses socket addresses without resolving
hostnames — so container/service DNS names do not work there and each node needs
a fixed address. Reach the handle with `state.extension::<ClusterHandle>()`; it
is `None` when the section is disabled, so a read path should degrade rather
than 500.

**App metrics** — call sites need no registration, but `describe_*` and
`set_histogram_buckets` must run before first use, since bucket bounds freeze at
registration; do it once at startup. A timer guard records on drop, so bind it
to a named variable and use `stop()` when you want the measurement to end before
the rest of the handler. Label values must come from a small closed set the code
owns.

## Testing helpers

Enable test support for integration-style app tests:

```toml
autumn-web = { version = "0.7", features = ["test-support"] }
```

Use `TestApp`, `TestClient`, `TestResponse`, and `TestDb` from
`autumn_web::test`. Doctests are still important because they compile public
examples from an external-consumer context.
`TestResponse` also supports CSS-selector assertions such as
`assert_selector`, `assert_no_selector`, `assert_selector_count`,
`assert_text`, `assert_text_contains`, and `assert_attr`.
