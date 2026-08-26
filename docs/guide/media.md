# Live Media (Broadcast + Rooms)

`autumn-media-plugin` adds live-streaming media to an `autumn-web` application.
It packages the two primitives an interactive streaming product needs, both
backed by [MediaMTX](https://github.com/bluenviron/mediamtx) for the actual
WebRTC / RTMP / HLS transport:

- **Broadcast** — one creator ingests (RTMP / WHIP / browser WebRTC) and a
  fan-out audience watches over low-latency WebRTC/WHEP with an HLS fallback;
  recordings become VODs plus clip/highlight/poster encodes.
- **Room** — a small **mesh** (no SFU) multi-participant call, capped at
  `room_max_participants` (default and absolute ceiling `6`). Each participant
  publishes its own WebRTC track and subscribes to every *other* participant.

The plugin owns the `[media]` config surface, the room signaling HTTP routes,
the `MediaMTX` control-API client and URL builders, the durable media-encode
jobs, and the recording-retention sweep. It never writes your application
tables — completed encodes are handed to an app-supplied callback (the
`MediaArtifactSink`), mirroring how the outbound-webhook plugin hands you a
delivery to record.

For a runnable end-to-end demo, see [`examples/media-room`](../../examples/media-room).

## Installation

```toml
[dependencies]
autumn-web = "0.7"
autumn-media-plugin = "0.7"
```

## Mounting the plugin

Autumn resolves config *after* `Plugin::build` runs, so the plugin cannot read
`[media]` from inside `build`. Instead you load a `MediaConfig` up front and
hand it to the builder — the same `from_config(&cfg) -> …` pattern
`autumn-storage-s3` uses. Enable each primitive explicitly with
`with_broadcast()` / `with_rooms()`:

```rust,ignore
use autumn_media_plugin::{prelude::*, MediaPlugin};

#[autumn_web::main]
async fn main() {
    let media = MediaConfig::from_autumn_toml("autumn.toml")
        .expect("valid [media] config");

    autumn_web::app()
        .plugin(
            MediaPlugin::new()
                .config(media)
                .with_broadcast()
                .with_rooms(),
        )
        .run()
        .await;
}
```

Both primitives are **off by default** — `MediaPlugin::new()` with neither
`with_broadcast()` nor `with_rooms()` mounts nothing. A rooms-only app calls
just `.with_rooms()`; a broadcast-only app (e.g. a one-to-many streaming site)
calls just `.with_broadcast()`.

### Builder options

`MediaPlugin` is a normal chainable builder. Beyond `config()` /
`with_broadcast()` / `with_rooms()`:

| Method | Effect |
|--------|--------|
| `.room_max_participants(usize)` | Per-room seat count (must be `1..=6`; mesh, no SFU). |
| `.room_token_ttl_seconds(u32)` | Lifetime of a minted room session token. |
| `.room_namespace(impl Into<String>)` | Optional `MediaMTX` path namespace isolating this deployment's rooms. |
| `.queue(impl Into<String>)` | Job queue the media encode jobs run on (default `media`). |
| `.api(impl Into<String>)` | URL prefix for the plugin's routes (default `/api/media`). |
| `.ffmpeg_bin(impl Into<String>)` | Override the `FFmpeg` binary path. |
| `.artifact_sink(Arc<dyn MediaArtifactSink>)` | App callback invoked when an encode completes. |
| `.retention_days(u32)` | Recording-retention window (`0` disables the sweep). |
| `.recordings_root(impl Into<PathBuf>)` | Filesystem root the retention sweep operates on (required to spawn it). |

An out-of-range `room_max_participants` (0 or > 6) or an invalid
`room_namespace` (a slash, a `.`/`..` segment, whitespace) is a **fail-fast**
boot error when rooms are enabled — never a silent clamp. A broadcast-only
plugin ignores stray room settings entirely.

## Configuration (`MediaConfig`)

`MediaConfig` models the `[media]` section of an Autumn profile. A minimal
localhost/dev config needs nothing — every field has a sensible default. A
production config selects S3 storage and points at your `MediaMTX` origins:

```toml
[media]
room_max_participants  = 6           # hard cap, mesh, no SFU (1..=6)
room_token_ttl_seconds = 300         # room session-token lifetime (seconds)
# room_namespace       = "tenant-a"  # optional MediaMTX path namespace
room_store_backend     = "memory"    # memory (default) | db

[media.mediamtx]
api_base            = "http://127.0.0.1:9997"
rtmp_base           = "rtmp://127.0.0.1:1935/live"
hls_base            = "http://127.0.0.1:8888"
hls_probe_base      = "http://mediamtx:8888"     # server-side probe; falls back to hls_base
webrtc_base         = "http://127.0.0.1:8889"
playback_base       = "http://127.0.0.1:9996"
playback_probe_base = "http://mediamtx:9996"     # server-side probe; falls back to playback_base

[media.ffmpeg]
bin = "/usr/bin/ffmpeg"

[media.storage]
backend           = "s3"             # local | s3   (default: local)
bucket            = "${MEDIA_BUCKET}"
endpoint_url      = "https://t3.storage.dev"
region            = "auto"
access_key_id     = "${MEDIA_S3_KEY}"
secret_access_key = "${MEDIA_S3_SECRET}"
public_base_url   = "https://cdn.example.com/media"
key_prefix        = "media"
force_path_style  = false

[media.recording]
retention_days = 14                  # 0 disables the retention sweep
```

### Profile-aware loading

Three loaders resolve a `MediaConfig`, all returning `MediaConfig::default()`
when no `[media]` table is present:

- `MediaConfig::from_autumn_toml("autumn.toml")` — read a single file, then
  apply `${VAR}` interpolation and `AUTUMN_MEDIA__<TABLE>__<FIELD>` overrides.
  It is a focused loader, **not** Autumn's full five-layer config resolver.
- `MediaConfig::from_autumn_dir(dir)` — the **profile-aware** loader. It resolves
  the active profile (`AUTUMN_ENV`) and merges the base `autumn.toml`'s `[media]`
  with any inline `[profile.<name>].media` block and the
  `autumn-<profile>.toml` override file — exactly as a deploy resolves it — so a
  `[profile.prod.media]` block is honored at runtime under `AUTUMN_ENV=prod`.
- `MediaConfig::from_arroyo_env()` — the migration shim (see below).

Call `media.validate()?` before mounting to fail fast on cross-field problems
(an out-of-range room cap, `backend = "s3"` without a `bucket`, or exactly one
of the S3 access-key/secret-key pair). The pure, testable cores
(`from_toml_str_with_env`, `from_autumn_dir_with_env`, `from_arroyo_env_pairs`)
take an explicit env map so config resolution can be unit-tested without
touching process-global state.

### Migrating from Arroyo

`MediaConfig::from_arroyo_env()` — and the one-call
`MediaPlugin::from_arroyo_env()` — map an existing Arroyo deployment's
`ARROYO_*` (plus `AWS_*` / `BUCKET_NAME`) environment onto a fully-wired
broadcast plugin, including the storage selection, `MediaMTX` origins, the
`FFmpeg` binary, the retention window, and the retention sweep's recordings
root (`ARROYO_RECORDINGS_ROOT`). Adopting the plugin changes no ops config; the
returned value is a normal builder you can layer further overrides on.

## Rooms

A **room** is an ephemeral rendezvous for a small mesh call. Participants join,
each publishes its own WebRTC track to a `MediaMTX` path and subscribes to every
*other* participant's path — a full mesh. Because the mesh is `O(N²)`, the room
is hard-capped at `DEFAULT_ROOM_MAX_PARTICIPANTS` (6); an SFU that would lift
that cap is out of scope.

### The room signaling routes

When `with_rooms()` is enabled, the plugin nests four HTTP routes under the API
prefix (default `/api/media`) and installs a `RoomService` on `AppState`:

| Method | Path | Purpose |
|--------|------|---------|
| `POST` | `/api/media/rooms` | Create a room; returns a token-free `RoomSnapshot`. |
| `POST` | `/api/media/rooms/{room_id}/join` | Join a room; returns a `JoinResponse` (session token + mesh transport targets). |
| `POST` | `/api/media/rooms/{room_id}/leave` | Leave a room (verifies the session token). |
| `GET`  | `/api/media/rooms/{room_id}` | The **member-gated** roster (`Authorization: Bearer <session token>`). |

> **Security:** these routes ship **no built-in authentication or rate
> limiting** on create/join — they **must** be mounted behind your
> application's own auth / rate-limit middleware. The plugin does not gate who
> may create or join a room. An `InMemoryRoomStore` caps the registry at 10,000
> rooms as a defense-in-depth backstop, and a background reaper reclaims idle
> rooms, but neither substitutes for your auth layer.

A `join` response gives the joiner its own `publish` target (the WHIP URL for
its `MediaMTX` path) plus one `subscribe` target (a WHEP URL) per existing peer.
The roster hands out per-peer WHEP endpoints, so it is **member-gated and
fail-closed**: a non-member (or a missing/bad `Authorization` header) gets the
same `404` as a nonexistent room — there is no membership oracle. Peer discovery
is by client polling of the roster (there is no WebSocket/SSE signaling channel
in this slice); a member re-polls to pick up later joiners.

`MediaMTX` maps each participant to a path — `room/{room_id}/{participant_id}`,
or `room/{namespace}/{room_id}/{participant_id}` when a `room_namespace` is set.
An operator enabling rooms must add a `path: "~^room/.+$"` matcher to
`mediamtx.yml` (alongside the broadcast `~^live/.+$` one) and allow the
`MediaMTX` WebRTC origin in the embedding page's `connect-src` CSP.

### Calling `RoomService` from your own handlers

The plugin installs a `RoomService` extension you can resolve from any handler
that has `State<AppState>`, so you can wrap the room lifecycle in your own
authenticated routes instead of exposing the raw plugin endpoints:

```rust,ignore
use autumn_web::extract::State;
use autumn_web::prelude::*;
use autumn_media_plugin::RoomService;

#[post("/rooms")]
async fn create_room(State(state): State<AppState>) -> AutumnResult<Json<serde_json::Value>> {
    let rooms = state
        .extension::<RoomService>()
        .ok_or_else(|| AutumnError::internal_server_error_msg("RoomService not installed"))?;

    let room = rooms.create().await.map_err(|e| e.into_autumn())?;
    Ok(Json(serde_json::json!({ "room_id": room.id })))
}
```

`RoomService` exposes `create()`, `join(room_id, display_name)`,
`leave(room_id, participant_id, token)`, and `roster(room_id, auth_token)` — the
same operations the built-in routes call. It is cheap to `Clone` (the store is
an `Arc`).

### Room store backends

Room state lives behind the `RoomStore` trait, selected by
`[media] room_store_backend`:

- **`memory`** (the default) — `InMemoryRoomStore`, single-process. Rooms live
  in one process's memory, so they vanish on restart and two app processes never
  see the same rooms. Correct for a single-node deployment or local development.
- **`db`** — `DbRoomStore`, multi-process safe. Rooms and participants are
  persisted in two tables (`media_rooms`, `media_room_participants`), so **every
  process sharing the database sees the same rooms** — the correct backend for a
  horizontally-scaled or multi-process deployment. Apply the plugin's
  `20260720000000_media_rooms` migration in any app that selects it. The tables
  are backend-portable (Postgres and SQLite): timestamps are `TIMESTAMP` (never
  `TIMESTAMPTZ`), and every query is written against autumn-web's
  `RuntimeConnection` alias. If `db` is selected without a configured database
  pool, the plugin logs an actionable error and degrades to the in-memory store
  so the app still boots.

Both backends enforce the absolute 6-seat mesh ceiling and cap the registry, and
a background reaper (`spawn_room_reaper_loop`) reclaims stale participants and
idle rooms by `last_seen_at` / `created_at`. The `DbRoomStore` reaper is a
last-write-wins sweep, so concurrent reapers across processes converge with no
corruption.

## Broadcast, transport, and encoding

### `MediaMtxClient` and `MediaUrls`

`with_broadcast()` wires the storage/encode surface. Two transport helpers
resolve from a `MediaMtxConfig`:

- **`MediaUrls::from_config(&config.mediamtx)`** builds every browser-facing and
  server-side URL a broadcast needs — `rtmp_ingest_url(key)`,
  `hls_playback_url(key)`, `webrtc_playback_url(key)`, `whip_publish_url(key)`,
  `whep_read_url(path)`, `recording_playback_url(...)`, and the internal
  probe variants — so playback/ingest URLs are composed in one place and never
  hand-built.
- **`MediaMtxClient::new(&config.mediamtx)`** talks to the `MediaMTX` control
  API: `fetch_ingest_statuses()`, `fetch_viewer_counts()`,
  `fetch_stream_status(key)`, `fetch_stream_quality(key)`, and
  `kick_publisher(key)`.

### Durable encode jobs

Broadcast recordings and room recordings turn into derived artifacts through
`MediaWorkflows`, installed on `AppState`. It queues durable jobs on the media
queue — `queue_thumbnail`, `queue_preview`, `queue_transcode`,
`queue_room_composite`, and `queue_recording_finalize` — each running `FFmpeg`.
The jobs run on autumn-web's built-in `#[job]` engine (no external workflow
engine is required); an optional `workflow_delegate` can route them through an
external durable engine instead.

When an encode completes, the plugin invokes the app-supplied
**`MediaArtifactSink`** — the callback you register with `.artifact_sink(...)` —
handing it a `MediaArtifact` (the produced files, the `source_id` you threaded
through the job args, and free-form metadata) so your app records the result
against its own schema. Without a sink, the artifact is persisted but the plugin
only logs that nothing recorded it — exactly like leaving the outbound-webhook
handler unset. This keeps the plugin out of your application tables.

## Recording retention

When a `recordings_root` is configured, the plugin spawns a background sweep
that deletes source recordings older than `retention_days` (0 disables it). Set
the root explicitly with `.recordings_root(...)` — without it, no sweep runs.
`MediaConfig::from_arroyo_env()` wires the root automatically from
`ARROYO_RECORDINGS_ROOT`.

## See also

- [`examples/media-room`](../../examples/media-room) — a minimal runnable app
  that installs the plugin with rooms and serves create/join/list routes.
- [Storage](storage.md) — the S3 / local blob-store backends media objects
  land in.
- [Custom subsystems](custom-subsystems.md) and
  [Extensibility](extensibility.md) — how the plugin trait and `AppState`
  extensions this plugin uses fit together.
