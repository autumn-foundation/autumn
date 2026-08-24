# autumn-media-plugin

`autumn-media-plugin` adds live-streaming media to an `autumn-web` application.
It packages the two primitives an interactive streaming product needs:

- **Broadcast** — one creator ingests (RTMP / WHIP / browser WebRTC) and a
  fan-out audience watches over low-latency WebRTC/WHEP with an HLS fallback,
  backed by `MediaMTX`; recordings become VODs plus clip/highlight encodes.
- **Room** — a small **mesh** (no SFU) multi-participant call, capped by
  `room_max_participants` (default `6`).

## Installation

```toml
[dependencies]
autumn-web = "0.7"
autumn-media-plugin = "0.7"
```

## `[media]` configuration

The plugin is configured by a `[media]` section in your Autumn profile:

```toml
[media]
room_max_participants  = 6           # hard cap, mesh, no SFU (1..=6)
room_token_ttl_seconds = 300         # room session-token lifetime
# room_namespace       = "tenant-a"  # optional MediaMTX path namespace

[media.mediamtx]
api_base            = "http://127.0.0.1:9997"
rtmp_base           = "rtmp://127.0.0.1:1935/live"
hls_base            = "http://127.0.0.1:8888"
hls_probe_base      = "http://mediamtx:8888"     # falls back to hls_base
webrtc_base         = "http://127.0.0.1:8889"
playback_base       = "http://127.0.0.1:9996"
playback_probe_base = "http://mediamtx:9996"     # falls back to playback_base

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
retention_days = 14                  # 0 disables the sweep
```

`${VAR}` placeholders resolve from the environment, and
`AUTUMN_MEDIA__<TABLE>__<FIELD>` variables override individual leaves.

## Mounting

Autumn resolves config *after* `Plugin::build` runs, so the plugin cannot read
`[media]` itself. Resolve a `MediaConfig` up front and pass it in — the same
`from_config(&cfg) -> …` pattern `autumn-storage-s3` uses:

```rust,ignore
use autumn_media_plugin::{prelude::*, MediaPlugin};

let media = MediaConfig::from_autumn_toml("autumn.toml")?;
media.validate()?;

autumn_web::app()
    .plugin(MediaPlugin::new().config(media).with_broadcast().with_rooms())
    .run()
    .await;
```

### Migrating from Arroyo

`MediaConfig::from_arroyo_env()` maps an existing Arroyo deployment's `ARROYO_*`
(and `AWS_*` / `BUCKET_NAME`) environment onto a `MediaConfig`, so an operator
changes nothing when adopting the plugin.

## Status

**Skeleton (slice 0).** This release ships the crate, the `MediaPlugin`
builder, the `MediaConfig` surface + `[media]` parsing, and the
`from_arroyo_env` compatibility shim. There is **no runtime behavior beyond
configuration and registration** — storage, encode, transport, and rooms land
in later slices.
