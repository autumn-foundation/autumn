# WebSockets in Autumn

Autumn exposes WebSocket endpoints through the `#[ws]` attribute macro, so
real-time routes use the same ergonomic shape as `#[get]` or `#[post]`:

```rust
use autumn_web::prelude::*;
use autumn_web::ws::{WebSocket, Message, WsHandler};

#[ws("/echo")]
async fn echo() -> impl WsHandler {
    |mut socket: WebSocket| async move {
        while let Some(Ok(Message::Text(t))) = socket.recv().await {
            socket.send(Message::Text(t)).await.ok();
        }
    }
}
```

Mount it the same way you would any other route:

```rust
#[autumn_web::main]
async fn main() {
    autumn_web::app()
        .routes(routes![echo])
        .run()
        .await;
}
```

Enable the feature in your `Cargo.toml`:

```toml
autumn-web = { version = "0.8", features = ["ws"] }
```

## The two-function pattern

A `#[ws]` handler is split into two phases:

1. **Outer function — runs at HTTP upgrade time.** This is where extractors
   (`State`, `Path`, `Query`, `AppState`) are resolved, authentication can
   be checked, and setup work (subscribing to a channel, looking up a user)
   happens before the socket is live.
2. **Returned closure — owns the live socket.** Autumn performs the HTTP to
   WebSocket upgrade and hands the closure a
   [`WebSocket`](https://docs.rs/axum/latest/axum/extract/ws/struct.WebSocket.html)
   it can read from and write to until the client disconnects.

Returning an error (or short-circuiting with `?`) from the outer function
rejects the upgrade with a standard HTTP status before the socket is ever
opened.

## Using extractors

Any Axum extractor that works on a `GET` handler also works on `#[ws]`:

```rust
use autumn_web::extract::{Path, Query};

#[ws("/rooms/{room}")]
async fn room(room: Path<String>) -> impl WsHandler {
    let name = room.to_string();
    move |mut socket: WebSocket| async move {
        socket.send(Message::Text(format!("joined {name}").into())).await.ok();
    }
}
```

`AppState` is special-cased: declare a parameter of type `AppState` and the
macro supplies it directly — no `State(...)` wrapper required.

```rust
#[ws("/chat")]
async fn chat(state: AppState) -> impl WsHandler {
    let channels = state.channels().clone();
    let tx = channels.sender("lobby");
    let mut rx = channels.subscribe("lobby");
    // ... return a closure that relays messages
}
```

## Graceful shutdown

For long-lived sockets, cooperate with Autumn's shutdown signal so the
server can drain cleanly. Wrap the closure in `WithShutdown` to receive a
`CancellationToken` alongside the socket:

```rust
use autumn_web::ws::{WithShutdown, CancellationToken};

#[ws("/feed")]
async fn feed(state: AppState) -> impl WsHandler {
    let mut rx = state.channels().subscribe("feed");
    WithShutdown(
        |mut socket: WebSocket, shutdown: CancellationToken| async move {
            loop {
                tokio::select! {
                    msg = rx.recv() => {
                        if let Ok(m) = msg {
                            if socket.send(Message::Text(m.into_string().into())).await.is_err() {
                                break;
                            }
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
```

## Fan-out with `Channels`

`AppState::channels()` returns a broadcast registry shared across all
handlers in the process. Use it to push the same message to every
connected client:

```rust
let channels = state.channels();
let tx = channels.sender("lobby");          // producer
let mut rx = channels.subscribe("lobby");   // consumer
```

Every `#[ws]` handler can own its own subscriber, so a single publish
fans out to every connected socket.

For SSE streams, htmx out-of-band HTML broadcasts, Redis-backed
multi-replica fan-out, and channel actuator metrics, see
[`realtime.md`](realtime.md).

## Limits

The `[realtime]` section limits WebSocket connections. It applies to every
`#[ws]` route.

```toml
[realtime]
max_connections = 5_000          # 503 with Retry-After above this
max_message_bytes = 1_048_576    # close code 1009 above 1 MiB
ping_interval_ms = 30_000        # server ping every 30 s
idle_timeout_ms = 120_000        # close code 1001 after 120 s of silence
```

| Key | Effect | Off |
|-----|--------|-----|
| `max_connections` | An upgrade above the limit gets `503` and `Retry-After: 1`. The count is per app. | unset or `0` |
| `max_message_bytes` | A larger received message closes the socket with code `1009`. `recv()` then returns the error, and then `None`. A single frame over 16 MiB also gets `1009`. | unset (64 MiB) |
| `ping_interval_ms` | The server sends a ping at this interval. The handler does not see the pong. | unset or `0` |
| `idle_timeout_ms` | When no complete message arrives for this long, the server sends close code `1001`. `recv()` then returns `None`. Pings, pongs and close frames count. The fragments of one message count only when the message is complete, so a client that sends a long message slowly must also send pings. | unset or `0` |

The `prod` profile sets `max_message_bytes`, `ping_interval_ms` and
`idle_timeout_ms` to the values above. It does not set `max_connections`.
`[server.http] max_connections` also limits WebSocket connections.
See [Server Connection Limits](connection-limits.md).

The ping and idle timers run only while the handler waits in `recv()` (or
`next()` on a split stream). A handler that never reads gets no pings and no
idle timeout. A handler that does long work between reads gets no pings
during that work, so a quiet client can reach the idle timeout.

When the stream ends (a close the server sends, the peer's close, or a
receive error), the socket closes its transport and frees its
`max_connections` slot. This also occurs when a `split()` sink half is still
alive in another task. A later send on that half returns an error.

The actuator `tasks/stream` socket does not use these limits.

`autumn_web::ws::WebSocket` is an Autumn type. It has the `recv`, `send` and
`protocol` methods of the axum socket, and a new `close` method. It
implements `Stream`, `FusedStream` and `Sink`, so `split()` works. To get the
axum socket, call `into_parts()`. It returns the axum socket and a
`ConnectionHold`. Keep the hold for the life of the socket: it holds the
`max_connections` slot and keeps an HTTP/2 connection out of its idle timer.
`into_inner()` drops the hold.

A handler on axum's own `WebSocketUpgrade` (for example in a router you add
with `merge_router`) has no `[realtime]` limits. Over HTTP/2, add the
`autumn_web::http_server::KeepTunnel` extractor and keep it for the life of
the socket. Without it, `keep_alive_timeout_ms` closes the connection while
the socket is open.

To set a different message limit on one route, write the upgrade yourself:

```rust,ignore
use autumn_web::ws::WebSocketUpgrade;

#[get("/upload")]
async fn upload(ws: WebSocketUpgrade) -> axum::response::Response {
    ws.max_message_size(16 * 1024 * 1024)
        .on_upgrade(|mut socket| async move {
            while let Some(Ok(_msg)) = socket.recv().await {}
        })
}
```

## Testing

See `examples/reddit-clone/src/routes/live.rs` for a runnable WebSocket live-feed
implementation and `autumn/tests/ws_integration.rs` for end-to-end tests that
drive real WebSocket traffic against an Autumn app using `tokio-tungstenite`.

## Out of scope

The `#[ws]` macro is a thin, ergonomic wrapper over Axum's WebSocket
support. It deliberately does **not** ship with:

- Application-level protocols (Socket.io, STOMP, GraphQL subscriptions)
- Durable replay or event persistence
- Client-side htmx extension bundling beyond Autumn's embedded htmx core

Build those on top when you need them; the primitives are here.
