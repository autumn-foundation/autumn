# Autumn Media Room Example

A minimal [`autumn-media-plugin`](../../autumn-media-plugin) application. It
installs the media plugin with the **rooms** primitive and serves a small
surface — a home page, a create-room form, and a JSON list endpoint — that call
the plugin's `RoomService` to create and list mesh-call rooms.

This is the runnable companion to the [Live Media guide](../../docs/guide/media.md).

## What it demonstrates

| Feature | Where | What it does |
|---------|-------|--------------|
| `MediaPlugin::new().config(media).with_rooms()` | `src/main.rs` | Installs the media plugin's rooms primitive |
| `RoomService` extension | `src/main.rs` | Creating a room from an app handler via `state.extension::<RoomService>()` |
| Plugin room routes | mounted under `/api/media` | `POST /rooms`, `POST /rooms/{id}/join`, `POST /rooms/{id}/leave`, `GET /rooms/{id}` |
| `AppState` extension | `src/main.rs` | A shared in-memory `RoomLog` powering the "list rooms" surface |

The plugin runs with its single-process defaults: local storage and the
in-memory room store. No database or `MediaMTX` server is required to boot this
example — the mesh WHIP/WHEP targets a `join` returns point at the default
localhost `MediaMTX` origins, which you would run separately to carry actual
media.

## Prerequisites

- Rust 1.88.0+

No database is required. To carry real WebRTC media you would additionally run a
[MediaMTX](https://github.com/bluenviron/mediamtx) server with a
`path: "~^room/.+$"` matcher, but the app boots and serves its routes without
one.

## Quick start

From the **workspace root** (`autumn/`):

```bash
cargo run -p media-room
```

The server starts on `http://localhost:3000`. Open it in a browser, click
**Create a room**, and the new room appears in the list.

### Prove it works

```bash
# Create a room (via this app's route, which calls RoomService):
curl -X POST http://localhost:3000/rooms -i
# => 303 See Other, Location: /

# List the rooms this app created:
curl http://localhost:3000/api/rooms
# => [{"id":"<uuid>","max_participants":6,"created_at":"..."}]

# Create a room directly through the plugin's mounted route:
curl -X POST http://localhost:3000/api/media/rooms
# => {"id":"<uuid>","max_participants":6,"created_at":"...","participants":[]}

# Join it (returns a session token + mesh WHIP/WHEP targets):
curl -X POST http://localhost:3000/api/media/rooms/<id>/join \
  -H 'content-type: application/json' -d '{"display_name":"Ada"}'
```

## Available routes

| Method | Path | Response |
|--------|------|----------|
| GET | `/` | Home page: create form + room list |
| POST | `/rooms` | Create a room, then redirect to `/` |
| GET | `/api/rooms` | JSON list of rooms this app created |
| POST | `/api/media/rooms` | (plugin) Create a room |
| POST | `/api/media/rooms/{room_id}/join` | (plugin) Join a room |
| POST | `/api/media/rooms/{room_id}/leave` | (plugin) Leave a room |
| GET | `/api/media/rooms/{room_id}` | (plugin) Member-gated roster |

## System smoke test

A headless-Chromium smoke lives in `tests/system/smoke.rs` (gated behind the
`system-tests` feature and `#[ignore]`, matching the other examples):

```bash
cargo test -p media-room --features system-tests --test smoke -- --include-ignored
```
