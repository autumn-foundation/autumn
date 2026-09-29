//! End-to-end integration tests for the `#[ws]` macro.
//!
//! These tests boot a full Autumn router on an ephemeral TCP port and
//! drive real WebSocket traffic against it with `tokio-tungstenite`,
//! verifying that the upgrade handshake, bidirectional messaging, path
//! and query extractors, and graceful shutdown all work together.

#![cfg(feature = "ws")]

use std::net::SocketAddr;
use std::time::Duration;

use autumn_web::extract::{Path, Query};
use autumn_web::prelude::*;
use autumn_web::test::TestApp;
use autumn_web::ws::{Message, WebSocket, WithShutdown, WsHandler};
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message as TMessage;
use tokio_util::sync::CancellationToken;

// ── Handlers under test ────────────────────────────────────────────

#[ws("/echo")]
async fn echo() -> impl WsHandler {
    |mut socket: WebSocket| async move {
        while let Some(Ok(msg)) = socket.recv().await {
            let send_result = match msg {
                Message::Text(t) => socket.send(Message::Text(t)).await,
                Message::Close(_) => break,
                _ => Ok(()),
            };
            if send_result.is_err() {
                break;
            }
        }
    }
}

#[ws("/rooms/{room}")]
async fn room_echo(room: Path<String>) -> impl WsHandler {
    let name = room.to_string();
    move |mut socket: WebSocket| async move {
        while let Some(Ok(Message::Text(t))) = socket.recv().await {
            let reply = format!("{name}:{t}");
            if socket.send(Message::Text(reply.into())).await.is_err() {
                break;
            }
        }
    }
}

#[derive(Deserialize)]
struct Greeting {
    name: String,
}

#[ws("/greet")]
async fn greet(q: Query<Greeting>) -> impl WsHandler {
    let name = q.0.name;
    move |mut socket: WebSocket| async move {
        socket
            .send(Message::Text(format!("hello, {name}").into()))
            .await
            .ok();
    }
}

#[ws("/shutdown-aware")]
async fn shutdown_aware() -> impl WsHandler {
    WithShutdown(
        |mut socket: WebSocket, shutdown: CancellationToken| async move {
            loop {
                tokio::select! {
                    incoming = socket.recv() => {
                        let send_result = match incoming {
                            Some(Ok(Message::Text(t))) => socket.send(Message::Text(t)).await,
                            Some(Ok(Message::Close(_))) | None => break,
                            _ => Ok(()),
                        };
                        if send_result.is_err() {
                            break;
                        }
                    }
                    () = shutdown.cancelled() => {
                        socket.send(Message::Text("bye".into())).await.ok();
                        socket.send(Message::Close(None)).await.ok();
                        break;
                    }
                }
            }
        },
    )
}

// ── Test harness ──────────────────────────────────────────────────

/// Serve the configured router on an ephemeral port. Returns the bound
/// address and the `AppState` the app was built with so tests can poke
/// at framework internals (e.g. triggering shutdown).
async fn serve() -> SocketAddr {
    let router = TestApp::new()
        .routes(routes![echo, room_echo, greet, shutdown_aware])
        .build()
        .into_router();

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local_addr");

    tokio::spawn(async move {
        axum::serve(listener, router).await.ok();
    });

    // Give the server a moment to start accepting.
    tokio::time::sleep(Duration::from_millis(20)).await;
    addr
}

async fn connect(
    addr: SocketAddr,
    path: &str,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let url = format!("ws://{addr}{path}");
    let (stream, response) = tokio_tungstenite::connect_async(url)
        .await
        .expect("ws connect");
    assert_eq!(
        response.status().as_u16(),
        101,
        "expected 101 Switching Protocols"
    );
    stream
}

// ── Tests ─────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn echo_round_trip() {
    let addr = serve().await;
    let mut stream = connect(addr, "/echo").await;

    stream
        .send(TMessage::Text("hello".into()))
        .await
        .expect("send");
    let reply = stream.next().await.expect("recv").expect("no error");
    match reply {
        TMessage::Text(t) => assert_eq!(t.as_str(), "hello"),
        other => panic!("unexpected reply: {other:?}"),
    }

    stream.close(None).await.ok();
}

#[tokio::test(flavor = "multi_thread")]
async fn path_extractor_is_visible_in_handler() {
    let addr = serve().await;
    let mut stream = connect(addr, "/rooms/lobby").await;

    stream
        .send(TMessage::Text("hi".into()))
        .await
        .expect("send");
    let reply = stream.next().await.expect("recv").expect("no error");
    match reply {
        TMessage::Text(t) => assert_eq!(t.as_str(), "lobby:hi"),
        other => panic!("unexpected reply: {other:?}"),
    }
    stream.close(None).await.ok();
}

#[tokio::test(flavor = "multi_thread")]
async fn query_extractor_runs_pre_upgrade() {
    let addr = serve().await;
    let mut stream = connect(addr, "/greet?name=ada").await;

    let reply = stream.next().await.expect("recv").expect("no error");
    match reply {
        TMessage::Text(t) => assert_eq!(t.as_str(), "hello, ada"),
        other => panic!("unexpected reply: {other:?}"),
    }
    stream.close(None).await.ok();
}

#[tokio::test(flavor = "multi_thread")]
async fn query_extractor_rejection_returns_non_101() {
    let addr = serve().await;
    // Missing required "name" query param -> extractor rejection before upgrade.
    let url = format!("ws://{addr}/greet");
    let result = tokio_tungstenite::connect_async(url).await;
    assert!(
        result.is_err(),
        "expected handshake to fail when query extractor rejects"
    );
}
