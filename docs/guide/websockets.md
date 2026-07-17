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
autumn-web = { version = "0.6", features = ["ws"] }
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
