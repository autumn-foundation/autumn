# Distributed Presence

Autumn's built-in presence primitive tells you which users are currently
subscribed to a given topic across all replicas. It is the Rust equivalent of
[Phoenix Presence](https://hexdocs.pm/phoenix/Phoenix.Presence.html) — per-topic
membership, automatic join/leave broadcasting, TTL-based lease eviction — exposed
as a single **request extractor** with no extra client-side dependencies.

For automatic per-record OOB broadcasts and the `--live` scaffold CLI flag,
see the [Realtime guide](realtime.md#auto-broadcast-with-livefragment).

## Why Presence?

Implementing "live viewers", "typing indicators", or "who is editing this record"
without a presence primitive means every team re-invents the same crash-prone
wheel:

- Track which `(user, connection)` pairs are joined to which topic.
- Expire them when sockets drop.
- Merge state across replicas.
- Broadcast joins/leaves to clients.

Presence handles all of that so you can ship collaborative UI features in minutes.

## Enabling Presence

Presence is part of the `ws` Cargo feature (same as `Channels`):

```toml
[dependencies]
autumn-web = { version = "0.5", features = ["ws"] }
```

It is available from `autumn_web::prelude::*` automatically when `ws` is enabled.

## The Contract

### `Presence` extractor

Declare `presence: Presence` in any handler and the framework injects the
shared process-level presence store:

```rust
use autumn_web::prelude::*;
use serde_json::json;

#[get("/rooms/{id}/viewers")]
async fn viewers(presence: Presence, path: Path<u64>) -> impl IntoResponse {
    let entries = presence.list(&format!("room:{}", *path));
    Json(entries)   // Vec<PresenceEntry> — works for any HTTP client
}
```

### `presence.track(topic, key, meta)` → `PresenceHandle`

Registers a presence entry:

| Argument | Type | Purpose |
|----------|------|---------|
| `topic`  | `impl Into<String>` | Namespace, e.g. `"room:42"` |
| `key`    | `impl Into<String>` | Stable identity, e.g. user ID |
| `meta`   | `impl Into<serde_json::Value>` | Arbitrary metadata |

Returns a [`PresenceHandle`] whose **`Drop` automatically removes the entry and
broadcasts a leave event**. Keep the handle alive as long as the connection is
open.

```rust
let _handle = presence.track("room:42", user_id.to_string(), json!({"name": "Alice"}));
// When _handle drops (end of scope, connection close, panic), the leave is automatic.
```

### `presence.list(topic)` → `Vec<PresenceEntry>`

Returns the current merged presence list for a topic. Multiple connections from
the same `key` are collapsed into one entry with one `meta` per connection
(Phoenix `Presence.list/1` semantics):

```rust
let entries = presence.list("room:42");
// [PresenceEntry { key: "alice", metas: [{"name": "Alice"}, {"name": "Alice (tab 2)"}] }]
```

### Lease / TTL model

Every tracked entry has a heartbeat timestamp. The background sweep (runs every
30 s by default) evicts entries whose heartbeat has not been refreshed within
the TTL (also 30 s by default). This handles the case where a process is killed
without `Drop` firing (e.g. `kill -9`, OOM kill):

```rust
// Refresh the heartbeat from your WebSocket ping loop:
loop {
    tokio::time::sleep(Duration::from_secs(15)).await;
    handle.refresh();   // extends the lease by another TTL window
}
```

## Join / Leave Events

Every `track()` call immediately publishes a JSON join event on the **derived
channel** `presence:{topic}`. Every `Drop` (or `sweep_expired()` eviction)
publishes a leave event. Existing `Channels` subscribers — including htmx SSE
and WebSocket clients — receive these events with no extra subscription
primitive.

**Event format** (both events have `"type"` set to `"join"` or `"leave"`):

```json
{ "type": "join",  "key": "alice", "meta": { "name": "Alice" } }
{ "type": "leave", "key": "alice" }
```

Subscribe with the standard `Channels` API:

```rust
let mut rx = state.channels().subscribe("presence:room:42");
let msg = rx.recv().await.unwrap();
let event: PresenceEvent = serde_json::from_str(msg.as_str()).unwrap();
```

Or with SSE:

```rust
#[get("/rooms/{id}/events")]
async fn events(State(state): State<AppState>, path: Path<u64>) -> impl IntoResponse {
    autumn_web::sse::stream(&state, &format!("presence:room:{}", *path))
}
```

## Backends

### In-process (default, single replica)

No configuration required. State is shared via `Arc<Mutex<…>>` within the
process. Ideal for development and single-instance production.

### Redis (multi-replica)

Set `channels.backend = "redis"` in your config. The existing Redis pub/sub
backend propagates join/leave events to all replicas so every replica's event
subscribers see the full picture. Each replica maintains a local in-memory
presence view and emits events through the shared Redis channel.

```toml
[channels]
backend = "redis"

[channels.redis]
url = "redis://127.0.0.1:6379"
```

Stale entries on a crashed replica are evicted within one TTL window (30 s)
by the background sweep on each surviving replica.

## Cookbook

### Active viewers badge (htmx + SSE)

The `examples/reddit-clone` example uses this pattern to show a "viewers on
this subreddit" badge. The essential pattern:

```rust
#[get("/posts/{id}/track")]
async fn track_viewer(
    State(state): State<AppState>,
    presence: Presence,
    path: Path<u32>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, std::convert::Infallible>>> {
    let topic = format!("post:{}", *path);

    // Subscribe BEFORE tracking so the first join event is never lost.
    let mut rx = state.channels().subscribe(&format!("presence:{topic}"));
    let handle = presence.track(&topic, "anonymous", serde_json::json!({}));

    let stream = async_stream::stream! {
        loop {
            match rx.recv().await {
                Ok(_) => {
                    let count = presence.list(&topic).len();
                    yield Ok(Event::default()
                        .event("viewer-badge")
                        .data(format!("{count} viewers")));
                }
                Err(_) => break,
            }
        }
        drop(handle); // leave is broadcast when stream ends
    };

    Sse::new(stream)
}
```

### Typing indicator

```rust
#[post("/rooms/{id}/typing")]
async fn start_typing(
    presence: Presence,
    auth: Auth<i64>,
    path: Path<i64>,
) -> impl IntoResponse {
    let topic = format!("room:{}", *path);
    let key   = format!("typing:{}", *auth);
    let meta  = serde_json::json!({ "user_id": *auth, "typing": true });

    // Caller must keep the returned handle alive (e.g. store in their state)
    // and drop it when the user stops typing.
    let handle = presence.track(&topic, key, meta);
    // Return the handle to the caller so they can drop it when appropriate.
    // In practice: store in a tokio::task-local or a shared map keyed by session.
    drop(handle); // simplified — real impl would store the handle
    StatusCode::NO_CONTENT
}
```

### Live cursors

Track each cursor position as metadata; re-track on every move to update `meta`:

```rust
fn track_cursor(presence: &Presence, doc_id: i64, user_id: i64, x: f32, y: f32)
    -> PresenceHandle
{
    presence.track(
        format!("doc:{doc_id}"),
        user_id.to_string(),
        serde_json::json!({ "x": x, "y": y }),
    )
}
```

Call `track_cursor` on every cursor-move message, letting the old handle drop.
The resulting join event carries the new coordinates; subscribers render
updated cursor positions.

### JSON API (framework-agnostic)

The presence API has no dependency on htmx:

```bash
curl http://localhost:3000/api/rooms/42/viewers
# [{"key":"alice","metas":[{"name":"Alice"}]},{"key":"bob","metas":[{"name":"Bob"}]}]
```

## API Reference

| Type / Method | Description |
|---------------|-------------|
| `Presence` | Request extractor; also available via `state.presence()` |
| `presence.track(topic, key, meta)` | Register presence, returns `PresenceHandle` |
| `presence.list(topic)` | Merged presence list for a topic |
| `presence.sweep_expired()` | Manually evict stale entries (called automatically every 30 s) |
| `PresenceHandle` | RAII guard; `Drop` = remove entry + broadcast leave |
| `handle.refresh()` | Extend the heartbeat lease |
| `handle.topic()` | Returns the tracked topic |
| `handle.key()` | Returns the tracked key |
| `PresenceEntry` | `{ key: String, metas: Vec<serde_json::Value> }` |
| `PresenceEvent` | `Join { key, meta }` or `Leave { key }` |

## Out of Scope (v1)

- CRDT-grade conflict resolution for concurrent metadata edits.
- Client-side JS diffing of presence updates.
- Multi-region active-active presence.
- Presence-based authorization.
