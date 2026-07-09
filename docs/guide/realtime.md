# Realtime Channels, SSE, and htmx Broadcasts

Autumn's `ws` feature provides a named channel registry for WebSockets, SSE,
and server-rendered htmx fragments. Local development uses in-process
`tokio::broadcast` channels. Multi-replica deployments can switch the same
API to Redis pub/sub with `autumn.toml`.

## Enable

```toml
[dependencies]
autumn-web = { version = "0.5", features = ["ws"] }
```

## Publish

Use `AppState::broadcast()` when the payload is intended for browser clients.
`publish` sends raw UTF-8 text. `publish_html` wraps a Maud fragment in an
`hx-swap-oob` envelope for htmx.

```rust,no_run
use autumn_web::prelude::*;

#[post("/tasks/{id}/complete")]
async fn complete(state: AppState, Path(id): Path<i64>) -> AutumnResult<&'static str> {
    state.broadcast().publish_html(
        "tasks",
        &html! {
            li id={ "task-" (id) } class="done" { "complete" }
        },
    )?;

    Ok("ok")
}
```

For protocol payloads that are already encoded, use `publish`:

```rust,no_run
# use autumn_web::prelude::*;
# fn publish(state: AppState) -> AutumnResult<()> {
state.broadcast().publish("tasks", br#"{"type":"task.completed"}"#.as_slice())?;
# Ok(())
# }
```

## Subscribe with SSE

The one-line SSE primitive subscribes to a topic and emits each message as
SSE `data`.

```rust,no_run
use autumn_web::prelude::*;

#[get("/events")]
async fn events(State(state): State<AppState>) -> impl IntoResponse {
    autumn_web::sse::stream(&state, "tasks")
}
```

Use `stream_authorized` when subscription needs access checks. The hook runs
before Autumn allocates the channel subscriber.

```rust,no_run
use autumn_web::prelude::*;

#[get("/events/private")]
async fn private_events(
    State(state): State<AppState>,
    session: Session,
) -> AutumnResult<impl IntoResponse> {
    autumn_web::sse::stream_authorized(&state, "private-tasks", |_| async move {
        if session.contains_key("user_id").await {
            Ok(())
        } else {
            Err(AutumnError::unauthorized_msg("login required"))
        }
    })
    .await
}
```

## Resumable SSE with `Last-Event-ID` (unreleased — trunk-dev, issue #1356)

`sse::stream_resumable` is a drop-in upgrade to `sse::stream` that survives a
brief client disconnect. Every event carries an automatic monotonic per-topic
`id`, and Autumn keeps a bounded per-topic replay ring buffer. When the browser
reconnects it echoes back the last id it saw in the `Last-Event-ID` header (the
SSE spec does this automatically); Autumn replays the events that arrived while
the client was gone, then continues live — with no duplicated or skipped events
at the seam.

```rust,no_run
use autumn_web::prelude::*;
use autumn_web::sse::LastEventId;

#[get("/events")]
async fn events(State(state): State<AppState>, LastEventId(last): LastEventId) -> impl IntoResponse {
    autumn_web::sse::stream_resumable(&state, "tasks", last)
}
```

- A first connection (no `Last-Event-ID`) behaves exactly like `sse::stream`:
  no replay, just live events.
- If the requested id has aged out of the retained window (the buffer
  overflowed while the client was away), the stream emits a single `gap`
  sentinel event (`event: gap`, `data: {"gap":true}`, no `id`) before the
  partial replay so the client can recover — for example by refetching the full
  list — instead of silently missing events.
- The existing id-less helpers (`sse::stream`, `sse::stream_authorized`,
  `sse::from_subscriber`) are unchanged; `stream_resumable` is additive.

Retention is configured with `channels.replay_buffer` (number of most-recent
events kept per topic; default `256`, env `AUTUMN_CHANNELS__REPLAY_BUFFER`).
Memory is `O(N)` per topic regardless of publish throughput. Only the in-process
backend retains a replay buffer; the Redis fan-out backend degrades gracefully
to live-only.

If you need the raw pieces (for a custom transport), `Channels::resume(topic,
last_event_id)` returns a `ResumeHandle { subscriber, replay, gap, next_live_id,
resumable }`.

### Limitations

The replay buffer is an in-process, best-effort convenience (the in-process
scope of issue #1356, matching its "Out of Scope") — not a durable event log:

- The buffer lives in the topic's in-memory state and is dropped when the topic
  is garbage-collected. A topic is kept only while it has a live receiver or an
  outstanding `Sender`, so a topic with only transient SSE subscribers can lose
  its buffer during the disconnect window and have nothing to replay on
  reconnect.
- On process restart the per-topic id counter resets to `1`, so a client
  reconnecting with a stale-large `Last-Event-ID` gets an empty replay and no
  gap sentinel.
- Publish via `channels.publish()` / `broadcast().publish()` on resumable
  topics. Calling `.send()` directly on a `Sender` from `channels.sender()`
  bypasses id assignment and the replay buffer, breaking resumability.

For durability across restarts or replicas, persist events yourself (a database
table or a Redis stream) rather than relying on this buffer.

## Direct Channels

`AppState::channels()` remains the low-level primitive for WebSocket loops and
custom transports.

```rust,no_run
# use autumn_web::prelude::*;
# async fn example(state: AppState) -> AutumnResult<()> {
let tx = state.channels().sender("lobby");
let mut rx = state.channels().subscribe_authorized("lobby", |_| async {
    Ok::<(), AutumnError>(())
}).await?;

tx.send("hello")?;
let _ = rx.recv().await;
# Ok(())
# }
```

## Redis Backend

Local is the default:

```toml
[channels]
backend = "in_process"
capacity = 32
```

Use Redis for multi-replica fan-out:

```toml
[channels]
backend = "redis"
capacity = 128

[channels.redis]
url = "redis://127.0.0.1:6379/"
key_prefix = "autumn:channels"
```

Equivalent environment overrides:

```powershell
$env:AUTUMN_CHANNELS__BACKEND = "redis"
$env:AUTUMN_CHANNELS__CAPACITY = "128"
$env:AUTUMN_CHANNELS__REDIS__URL = "redis://127.0.0.1:6379/"
$env:AUTUMN_CHANNELS__REDIS__KEY_PREFIX = "autumn:channels"
```

The Redis backend publishes locally first, then relays the same envelope over
Redis. Each process ignores messages carrying its own origin id, which avoids
double-delivery on the publishing replica.

## Custom Backends

Implement `autumn_web::channels::ChannelsBackend` and install it with
`AppBuilder::with_channels_backend`. This bypasses config-driven backend
selection, matching the session-store escape hatch.

```rust,no_run
# use autumn_web::prelude::*;
# fn configure(app: autumn_web::app::AppBuilder) -> autumn_web::app::AppBuilder {
app.with_channels_backend(LocalChannelsBackend::new(64))
# }
```

## Auto-broadcast with `LiveFragment`

Autumn can broadcast OOB (out-of-band) htmx fragments automatically whenever
a repository mutates a record. Opt in by implementing `LiveFragment` on your
model and adding `broadcasts = true` to `#[repository]`.

Requires the `ws`, `maud`, and `htmx` Cargo features.

### 1. Implement `LiveFragment`

```rust
use autumn_web::prelude::*;
use maud::{Markup, html};

pub struct Post { pub id: i64, pub title: String }

impl LiveFragment for Post {
    fn dom_id_for(id: i64) -> String {
        format!("post-{id}")
    }

    fn dom_id(&self) -> String {
        Self::dom_id_for(self.id)
    }

    fn render_fragment(&self) -> Markup {
        html! {
            li id=(self.dom_id()) { (self.title) }
        }
    }

    // Override for inserts: append to a list container instead of
    // trying to replace an element that doesn't exist yet on the client.
    fn insert_swap() -> OobSwap {
        OobSwap::Target(OobMethod::BeforeEnd, "#posts-list".to_string())
    }
}
```

### 2. Declare `broadcasts = true` on the repository

```rust
#[repository(Post, broadcasts = true)]
pub trait PostRepository {}
```

Use `topic = "custom-name"` to override the default topic (which is the
table name, e.g. `"posts"`). Broadcasts fire synchronously after each
`save`, `update`, or `delete_by_id` call.

### 3. Wire the SSE list container in your template

```html
<ul id="posts-list"
    hx-ext="sse"
    sse-connect="/posts/stream"
    sse-swap="message"
    hx-swap="none">
  <!-- rows rendered server-side on initial load -->
</ul>
```

`hx-swap="none"` is required — it disables htmx's default in-band
`innerHTML` swap so that incoming SSE messages are processed only as OOB
swaps (instead of clearing the list first).

### 4. Add the SSE stream route

```rust
#[get("/posts/stream")]
async fn stream(State(state): State<AppState>) -> impl IntoResponse {
    autumn_web::sse::stream(&state, "posts")
}
```

### OOB swap strategies

| Mutation | Default strategy |
|----------|-----------------|
| `save`   | `LiveFragment::insert_swap()` — defaults to `OobSwap::True` (replace by id); override with `OobSwap::Target` to append to a container |
| `update` | `OobSwap::OuterHTML` (replace element by matching id) |
| `delete` | `OobSwap::Delete` (remove element from DOM) |

When both `broadcasts = true` and `commit_hooks = true` are declared,
broadcasts fire in the durable commit-hook worker (after DB commit). Without
commit hooks, they fire inline after the mutation in the same async task.

---

## Live scaffold — the closest thing to Phoenix LiveView

`autumn generate scaffold` with `--live` and `--live-validation` gives you
the same two headline features that make Phoenix LiveView compelling:
real-time DOM updates pushed from the server, and inline field validation
without a page reload — all without a persistent socket or a client-side
state machine.

### The one-liner

```sh
autumn generate scaffold Post title:String body:String \
    --live \
    --live-validation \
    --validate title=length:min=1,max=200 \
    --validate body=length:min=1
```

Open two browser tabs on the index. Create, edit, or delete a post in one
tab — the other tab updates itself via SSE without polling or a full reload.
Type in the create/edit form — error messages appear inline, per field, on
`change`, driven by the same server-side rules that guard the actual save.

### What `--live` generates

- A `LiveFragment` impl on the model (see above).
- `#[repository(Post, broadcasts = true)]` on the repository — save, update,
  and delete automatically publish OOB htmx fragments to connected clients.
- A `/posts/events` SSE route.
- The idiomorph `<script>` in the layout (`IDIOMORPH_JS_PATH`) and
  `hx-ext="morph"` on `<body>` for smooth morphing navigations.
- An index list container wired to the SSE stream:

```html
<ul id="posts-list"
    hx-ext="sse"
    sse-connect="/posts/events"
    sse-swap="message"
    hx-swap="none">
```

`hx-swap="none"` is intentional — it prevents htmx from running an
in-band innerHTML swap (which would clear the list) when an SSE message
arrives. The OOB attributes on each fragment handle their own targeted
patch.

### What `--live-validation` adds

This is the key feature. For every field named in `--validate`, the
scaffold generates three things working together:

**1. `hx-*` attributes on each form input** (both create and edit forms):

```html
<input name="title" type="text" value=""
       hx-post="/posts/validate/title"
       hx-trigger="change"
       hx-target="#title-error"
       hx-swap="outerHTML">
```

On every `change` event the browser POSTs the current field value to a
dedicated validation endpoint. No JavaScript required.

**2. An error span slot** adjacent to each input:

```html
<span id="title-error"></span>
```

htmx's `hx-swap="outerHTML"` replaces this span with whatever the server
returns — either a populated error span or an empty one on success.

**3. A validation handler** per field that runs the actual declared rules:

```rust
#[post("/posts/validate/title")]
pub async fn validate_title(body: Bytes) -> Markup {
    let value = /* parse form body */;
    let error: Option<&str> = if value.is_empty() {
        Some("required")
    } else if value.chars().count() > 200 {
        Some("must be at most 200 characters")
    } else {
        None
    };
    html! {
        span id="title-error" {
            @if let Some(msg) = error {
                span style="color:red" { (msg) }
            }
        }
    }
}
```

The rules are the same ones the model's `#[validate]` attributes enforce on
save — they live in one place (the `--validate` flags) and the scaffold
emits them consistently in both the model and the runtime handler. Users see
feedback at `change` time; the server rejects invalid saves anyway; no
duplication of logic.

### Supported validation rules

| Rule | Flag syntax | Behaviour |
|------|-------------|-----------|
| Required (non-nullable) | automatic | Empty string → `"required"` |
| Min/max length | `length(min=1,max=200)` | Character count check |
| Valid URL | `url` | `url::Url::parse` |
| Valid email | `email` | `@` + domain dot check |

### Comparison with Phoenix LiveView

Phoenix LiveView achieves inline validation through a persistent WebSocket
and a stateful server-side process that re-renders the form on each event.
Autumn's approach is intentionally stateless — each validation POST is a
standalone HTTP request that returns a single fragment. There is no
per-connection server process to crash, hibernate, or scale; the tradeoff
is that multi-field cross-validation (e.g. "password matches confirm")
requires a small custom handler rather than a single `handle_event`.

---

## Presence helpers

See the [Presence guide](presence.md) for `presence_stream` (an SSE stream
of join/leave events with OOB count badges) and `presence_badge` (a
re-usable viewer-count fragment). Both integrate with the same channels
infrastructure described here.

---

## Actuator Metrics

With the `ws` feature, `/actuator/channels` returns per-topic metrics:

```json
{
  "channels": {
    "tasks": {
      "subscriber_count": 2,
      "lifetime_publish_count": 17,
      "dropped_count": 0,
      "lagged_count": 1
    }
  }
}
```

`dropped_count` increments when a publish has no active local receivers.
`lagged_count` increments when slow subscribers skip messages from the
bounded ring buffer.

## Two-Replica Smoke

The dedicated two-replica smoke test that lived in `examples/ws-echo` has been
consolidated into `examples/reddit-clone`, which demonstrates the same
Redis pub/sub fan-out pattern through its live-feed WebSocket route
(`src/routes/live.rs`). The reddit-clone Docker Compose file only starts the
Postgres and Redis infrastructure; to run a multi-replica smoke you would
start two app instances manually (each pointing at the same Redis and
Postgres) and verify that a post created through one replica appears on a
WebSocket connection opened against the other.
