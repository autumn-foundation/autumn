# In-app notifications

Every account-based app eventually grows a notification bell: a persistent,
per-user feed with read/unread state and an unread count. Autumn ships that
composition as a first-class primitive — the [`Notifications`] service — so you
add a working feed with one generate command and one `notify(...)` call, with
zero hand-written SQL.

This is the **in-app/database channel** only: it is not
[flash](flash.md) (transient, single-request) and not the mailer (email is a
different delivery channel).

## Quick start

Scaffold the table, a minimal feed API, and a smoke test:

```bash
autumn generate notifications
autumn migrate
```

The generator emits a backend-aware migration (Postgres or SQLite, matching
your app's configured backend), a `src/notifications.rs` module with feed
routes, registers those routes in `main.rs`, and writes an in-process smoke
test (`tests/notifications_feed.rs`) that exercises notify → list → mark-read
through `TestClient` — run it with `cargo test`.

Then notify from anywhere a handler runs:

```rust
use autumn_web::notifications::Notifications;
use autumn_web::prelude::*;

#[post("/comments")]
async fn create_comment(notifications: Notifications) -> AutumnResult<&'static str> {
    // ... create the comment ...
    notifications
        .notify(recipient_id, "comment.created", serde_json::json!({ "post": 42 }))
        .await?;
    Ok("ok")
}
```

## The API

`Notifications` is an extractor, surfaced the same way as `Session` and `Db`:
declare it as a handler parameter. It provides:

| Method | Behavior |
|--------|----------|
| `notify(recipient_id, kind, payload)` | Persist a new unread notification; immediately visible to reads |
| `list(recipient_id, &ListQuery, &PageRequest)` | One `Page` of the feed (shipped pagination) |
| `unread_count(recipient_id)` | Count of rows with `read_at` unset |
| `mark_read(id)` / `mark_read_for(recipient_id, id)` | Mark one read — idempotent; the `_for` variant refuses to touch other recipients' rows |
| `mark_all_read(recipient_id)` | Mark everything read; returns how many transitioned; idempotent |

Reading the feed reuses `Page`/`ListQuery`/`PageRequest`, so the usual query
parameters work out of the box: `?page=2&size=20`, `?filter[unread]=true`,
`?filter[kind]=comment.created`, `?sort=created_at&dir=asc`. Without an
explicit sort the feed returns newest-first. Unknown filters and sort keys are
ignored, matching the repository `list()` contract.

```rust
use autumn_web::notifications::{Notification, Notifications};
use autumn_web::prelude::*;

#[get("/notifications")]
async fn feed(
    Auth(user): Auth<CurrentUser>,
    query: ListQuery,
    page: PageRequest,
    notifications: Notifications,
) -> AutumnResult<Json<Page<Notification>>> {
    Ok(Json(notifications.list(user.id, &query, &page).await?))
}
```

In user-facing handlers prefer `mark_read_for(user.id, id)` over
`mark_read(id)`: it scopes the update to the signed-in recipient, so a user
cannot mark someone else's notification read by guessing ids.

## Storage

`Notifications` delegates to a pluggable `NotificationStore`, resolved once
per app:

1. A store you registered via `AppBuilder::with_notification_store(...)`.
2. `DbNotificationStore` when a database pool is configured — the persistent
   default, backed by the `notifications` table the generator scaffolds.
3. `MemoryNotificationStore` otherwise — a process-local fallback for tests
   and DB-less development (contents are lost on restart, and the store grows
   without bound — do not ship it as the production store).

The `notifications` table stores `id`, `recipient_id`, `kind`, a JSON
`payload` (TEXT-serialized, so it is identical on Postgres and SQLite), a
nullable `read_at`, and `created_at`. Timestamps are `TIMESTAMPTZ` on
Postgres and RFC 3339 `TEXT` on SQLite. Keep payloads small and
reference-shaped (`{"post": 42}`, not the post body): they are stored
verbatim per recipient and re-sent on every feed page.

To plug in a custom backend, implement `NotificationStore` and register it:

```rust
autumn_web::app()
    .with_notification_store(MyStore::new())
    .run()
    .await;
```

## Realtime push over channels (optional, best-effort)

With the `ws` feature enabled you can push each new notification to connected
clients over the existing [channels](realtime.md) transport. The conventional
per-recipient topic is `Notifications::topic(recipient_id)`
(`"notifications:{recipient_id}"`).

The built-in helper persists and then publishes the stored notification as
JSON:

```rust
#[post("/likes")]
async fn like(notifications: Notifications) -> AutumnResult<&'static str> {
    // Persists, then publishes on "notifications:{recipient}". The broadcast
    // is best-effort: a channel failure never fails the notify.
    notifications
        .notify_with_push(recipient_id, "like", serde_json::json!({}))
        .await?;
    Ok("ok")
}
```

Or wire it manually — the important part is ignoring the publish result, so a
dead channel (or simply "no subscribers right now") never fails the write:

```rust
let n = notifications.notify(user.id, "like", payload).await?;
let _ = state
    .broadcast()
    .publish(&Notifications::topic(user.id), serde_json::to_string(&n)?);
```

A WebSocket or SSE handler subscribes to the same topic to stream the feed:

```rust
// SECURITY: derive the topic from the *authenticated* user (Auth/session),
// never from a client-supplied id — topics are guessable and the pushed JSON
// contains the full notification payload. For channel-level enforcement use
// the authorized subscription helpers (`subscribe_authorized` /
// `sse::stream_authorized`, see the realtime guide).
let mut rx = state.channels().subscribe(&Notifications::topic(user.id));
while let Ok(msg) = rx.recv().await {
    // forward msg (the notification JSON) to the client
}
```

## Out of scope (by design)

The bell/dropdown widget itself, email/SMS delivery, per-user notification
preferences, digests, and cross-recipient fan-out are deliberately not part of
this primitive — it ships the data + API layer that those can build on.

[`Notifications`]: https://docs.rs/autumn-web/latest/autumn_web/notifications/struct.Notifications.html
