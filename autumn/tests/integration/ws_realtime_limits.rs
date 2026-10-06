//! `[realtime]` WebSocket limits (issue #3065).

#![cfg(feature = "ws")]

use std::net::SocketAddr;
use std::time::Duration;

use autumn_web::config::AutumnConfig;
use autumn_web::prelude::*;
use autumn_web::test::TestApp;
use autumn_web::ws::{Message, WebSocket, WsHandler};
use futures::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message as TMessage;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

type Client =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Echo text. Report any other frame the handler sees as text, so a test can
/// see frames the wrapper must hide.
#[ws("/limited")]
async fn limited() -> impl WsHandler {
    |mut socket: WebSocket| async move {
        while let Some(Ok(msg)) = socket.recv().await {
            let reply = match msg {
                Message::Text(t) => Message::Text(t),
                Message::Close(_) => break,
                other => Message::Text(format!("unexpected: {other:?}").into()),
            };
            if socket.send(reply).await.is_err() {
                break;
            }
        }
    }
}

/// A `split()` handler whose writer half waits for work that never comes.
#[ws("/split")]
async fn split_writer() -> impl WsHandler {
    |socket: WebSocket| async move {
        let (sink, mut stream) = socket.split();
        tokio::spawn(async move {
            let _sink = sink;
            std::future::pending::<()>().await;
        });
        while let Some(Ok(_)) = stream.next().await {}
    }
}

/// A handler on the axum socket. It keeps the hold for the socket's life.
#[get("/raw")]
async fn raw(ws: autumn_web::ws::WebSocketUpgrade) -> axum::response::Response {
    let (upgrade, hold) = ws.into_parts();
    upgrade.on_upgrade(move |mut socket| async move {
        let _hold = hold;
        while let Some(Ok(_)) = socket.recv().await {}
    })
}

async fn serve_with(configure: impl FnOnce(&mut AutumnConfig)) -> SocketAddr {
    let mut config = AutumnConfig::default();
    configure(&mut config);
    let router = TestApp::new()
        .config(config)
        .routes(routes![limited, raw, split_writer])
        .build()
        .into_router();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.ok();
    });
    addr
}

async fn connect(addr: SocketAddr) -> Client {
    let (stream, response) = tokio_tungstenite::connect_async(format!("ws://{addr}/limited"))
        .await
        .expect("ws connect");
    assert_eq!(response.status().as_u16(), 101);
    stream
}

/// Read frames until a close frame arrives. Fails on timeout.
async fn next_close(client: &mut Client) -> Option<(CloseCode, String)> {
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(5), client.next())
            .await
            .expect("close frame in time");
        match frame {
            Some(Ok(TMessage::Close(frame))) => {
                return frame.map(|f| (f.code, f.reason.to_string()));
            }
            Some(Ok(TMessage::Text(t))) => panic!("unexpected text: {t}"),
            Some(Ok(_)) => {}
            Some(Err(e)) => panic!("socket error before close frame: {e}"),
            None => panic!("stream ended without a close frame"),
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn message_over_max_message_bytes_closes_with_1009() {
    let addr = serve_with(|c| c.realtime.max_message_bytes = Some(1024)).await;
    let mut client = connect(addr).await;

    client.send(TMessage::text("small")).await.unwrap();
    let echo = client.next().await.unwrap().unwrap();
    assert_eq!(echo, TMessage::text("small"));

    client.send(TMessage::text("x".repeat(2048))).await.unwrap();
    let (code, _) = next_close(&mut client)
        .await
        .expect("close frame has a code");
    assert_eq!(code, CloseCode::Size, "expected 1009 Message Too Big");
}

#[tokio::test(flavor = "multi_thread")]
async fn connections_beyond_max_connections_are_rejected() {
    let addr = serve_with(|c| c.realtime.max_connections = Some(1)).await;
    let mut first = connect(addr).await;

    let err = tokio_tungstenite::connect_async(format!("ws://{addr}/limited"))
        .await
        .expect_err("second socket must be rejected at the cap");
    match err {
        tokio_tungstenite::tungstenite::Error::Http(response) => {
            assert_eq!(response.status().as_u16(), 503);
            assert!(response.headers().contains_key("retry-after"));
        }
        other => panic!("expected an HTTP rejection, got {other}"),
    }

    // Closing the first socket frees its slot.
    first.close(None).await.ok();
    drop(first);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if tokio_tungstenite::connect_async(format!("ws://{addr}/limited"))
            .await
            .is_ok()
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "slot not released after close"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn server_pings_at_ping_interval_and_hides_the_pongs() {
    let addr = serve_with(|c| c.realtime.ping_interval_ms = Some(100)).await;
    let mut client = connect(addr).await;

    let mut pings = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_millis(800);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout_at(deadline, client.next()).await {
            Err(_) => break,
            Ok(Some(Ok(TMessage::Ping(_)))) => pings += 1,
            Ok(Some(Ok(TMessage::Text(t)))) => panic!("handler saw a frame: {t}"),
            Ok(other) => panic!("unexpected: {other:?}"),
        }
    }
    assert!(pings >= 3, "expected pings every 100ms, got {pings}");

    // The socket still works after the pongs.
    client.send(TMessage::text("still here")).await.unwrap();
    loop {
        match client.next().await.unwrap().unwrap() {
            TMessage::Ping(_) => {}
            other => {
                assert_eq!(other, TMessage::text("still here"));
                break;
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn silent_peer_is_closed_after_idle_timeout() {
    let addr = serve_with(|c| c.realtime.idle_timeout_ms = Some(300)).await;
    let mut client = connect(addr).await;
    let start = tokio::time::Instant::now();
    let (code, _) = next_close(&mut client)
        .await
        .expect("close frame has a code");
    assert_eq!(code, CloseCode::Away, "expected 1001 Going Away");
    assert!(start.elapsed() >= Duration::from_millis(200));
}

#[tokio::test(flavor = "multi_thread")]
async fn no_limits_set_keeps_large_messages_working() {
    let addr = serve_with(|_| {}).await;
    let mut client = connect(addr).await;
    let big = "y".repeat(256 * 1024);
    client.send(TMessage::text(big.clone())).await.unwrap();
    assert_eq!(client.next().await.unwrap().unwrap(), TMessage::text(big));
}

#[tokio::test(flavor = "multi_thread")]
async fn into_parts_keeps_the_connection_slot() {
    let addr = serve_with(|c| c.realtime.max_connections = Some(1)).await;
    let (_first, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/raw"))
        .await
        .expect("first socket");
    let err = tokio_tungstenite::connect_async(format!("ws://{addr}/raw"))
        .await
        .expect_err("the axum socket still holds the only slot");
    match err {
        tokio_tungstenite::tungstenite::Error::Http(response) => {
            assert_eq!(response.status().as_u16(), 503);
        }
        other => panic!("expected an HTTP rejection, got {other}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_close_releases_a_split_socket() {
    let addr = serve_with(|c| {
        c.realtime.max_connections = Some(1);
        c.realtime.idle_timeout_ms = Some(200);
    })
    .await;
    // This client never reads, so it never answers the close frame.
    let (_silent, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/split"))
        .await
        .expect("first socket");
    // After the idle close, the slot is free, although the writer half lives.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if tokio_tungstenite::connect_async(format!("ws://{addr}/split"))
            .await
            .is_ok()
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the closed socket still holds the slot"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
