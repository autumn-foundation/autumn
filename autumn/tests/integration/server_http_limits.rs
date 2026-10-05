//! `[server.http]` connection limits on the real serve loop (issue #3065).

use std::net::SocketAddr;
use std::time::Duration;

use autumn_web::http_server::{HttpLimits, serve};
use axum::Router;
use axum::routing::get;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::Instant;

async fn slow_handler() -> &'static str {
    tokio::time::sleep(Duration::from_millis(600)).await;
    "slow"
}

async fn spawn_server(limits: HttpLimits) -> SocketAddr {
    let router = Router::new()
        .route("/", get(|| async { "ok" }))
        .route("/slow", get(slow_handler));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(serve(
        listener,
        router.into_make_service(),
        limits,
        std::future::pending(),
    ));
    addr
}

#[allow(clippy::unnecessary_wraps, reason = "matches the `HttpLimits` fields")]
const fn ms(value: u64) -> Option<Duration> {
    Some(Duration::from_millis(value))
}

/// Read until the peer closes. Returns the time it took. A reset counts as a
/// close.
async fn time_until_closed(stream: &mut TcpStream, limit: Duration) -> Duration {
    let start = Instant::now();
    let mut buf = [0_u8; 1024];
    loop {
        match tokio::time::timeout(limit, stream.read(&mut buf)).await {
            Err(elapsed) => panic!("connection still open: {elapsed}"),
            Ok(Ok(0) | Err(_)) => return start.elapsed(),
            Ok(Ok(_)) => {}
        }
    }
}

/// Read one `Content-Length` response. Returns the head and the body.
async fn read_response(stream: &mut TcpStream) -> (String, String) {
    let mut data = Vec::new();
    let mut buf = [0_u8; 4096];
    loop {
        let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf))
            .await
            .expect("response in time")
            .expect("read ok");
        assert!(n > 0, "closed before a full response: {data:?}");
        data.extend_from_slice(&buf[..n]);
        let text = String::from_utf8_lossy(&data).to_string();
        if let Some(end) = text.find("\r\n\r\n") {
            let head = text[..end].to_owned();
            let len = head
                .lines()
                .find_map(|l| {
                    l.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|v| v.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            if text.len() >= end + 4 + len {
                return (head, text[end + 4..end + 4 + len].to_owned());
            }
        }
    }
}

const GET_ROOT: &[u8] = b"GET / HTTP/1.1\r\nHost: test\r\n\r\n";

#[tokio::test]
async fn slow_header_client_is_disconnected_after_header_read_timeout() {
    let addr = spawn_server(HttpLimits {
        header_read_timeout: ms(500),
        ..HttpLimits::default()
    })
    .await;
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: test\r\n")
        .await
        .unwrap();
    let closed_after = time_until_closed(&mut stream, Duration::from_secs(5)).await;
    assert!(
        closed_after >= Duration::from_millis(300),
        "closed too early: {closed_after:?}"
    );
}

#[tokio::test]
async fn trickled_header_bytes_do_not_extend_the_header_read_timeout() {
    let addr = spawn_server(HttpLimits {
        header_read_timeout: ms(500),
        ..HttpLimits::default()
    })
    .await;
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let start = Instant::now();
    let closed = async {
        for byte in b"GET / HTTP/1.1\r\nX-Slow: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" {
            if stream.write_all(&[*byte]).await.is_err() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!(
            "server still accepts header bytes after {:?}",
            start.elapsed()
        );
    };
    closed.await;
    assert!(start.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn silent_client_is_disconnected_after_header_read_timeout() {
    let addr = spawn_server(HttpLimits {
        header_read_timeout: ms(500),
        ..HttpLimits::default()
    })
    .await;
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let closed_after = time_until_closed(&mut stream, Duration::from_secs(5)).await;
    assert!(closed_after >= Duration::from_millis(300));
}

#[tokio::test]
async fn header_read_timeout_does_not_cut_a_slow_handler() {
    let addr = spawn_server(HttpLimits {
        header_read_timeout: ms(200),
        keep_alive_timeout: ms(200),
        ..HttpLimits::default()
    })
    .await;
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(b"GET /slow HTTP/1.1\r\nHost: test\r\n\r\n")
        .await
        .unwrap();
    let (head, body) = read_response(&mut stream).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert_eq!(body, "slow");
}

#[tokio::test]
async fn idle_keep_alive_connection_is_closed_after_keep_alive_timeout() {
    let addr = spawn_server(HttpLimits {
        keep_alive_timeout: ms(500),
        ..HttpLimits::default()
    })
    .await;
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(GET_ROOT).await.unwrap();
    let (head, _) = read_response(&mut stream).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    // The connection stays open for a second request inside the window.
    stream.write_all(GET_ROOT).await.unwrap();
    let (head, _) = read_response(&mut stream).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    let closed_after = time_until_closed(&mut stream, Duration::from_secs(5)).await;
    assert!(closed_after >= Duration::from_millis(300));
}

#[tokio::test]
async fn header_over_max_header_bytes_gets_431() {
    let addr = spawn_server(HttpLimits {
        max_header_bytes: Some(8192),
        ..HttpLimits::default()
    })
    .await;
    let mut stream = TcpStream::connect(addr).await.unwrap();
    // Exactly the limit and no end of head: the server reads every byte, so
    // it closes with FIN, not RST, and the client can read the 431.
    let mut request = b"GET / HTTP/1.1\r\nHost: test\r\nX-Big: ".to_vec();
    request.resize(8192, b'a');
    stream.write_all(&request).await.unwrap();
    let mut buf = vec![0_u8; 1024];
    let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf))
        .await
        .unwrap()
        .unwrap();
    let text = String::from_utf8_lossy(&buf[..n]);
    assert!(text.starts_with("HTTP/1.1 431"), "{text}");
}

#[tokio::test]
async fn max_connections_holds_extra_connections_until_one_closes() {
    let addr = spawn_server(HttpLimits {
        max_connections: Some(1),
        ..HttpLimits::default()
    })
    .await;
    let mut first = TcpStream::connect(addr).await.unwrap();
    first.write_all(GET_ROOT).await.unwrap();
    let (head, _) = read_response(&mut first).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");

    let mut second = TcpStream::connect(addr).await.unwrap();
    second.write_all(GET_ROOT).await.unwrap();
    let mut buf = [0_u8; 64];
    let early = tokio::time::timeout(Duration::from_millis(300), second.read(&mut buf)).await;
    assert!(early.is_err(), "second connection served above the cap");

    drop(first);
    let (head, _) = read_response(&mut second).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
}

#[tokio::test]
async fn http2_settings_advertise_max_concurrent_streams() {
    let addr = spawn_server(HttpLimits {
        http2_max_concurrent_streams: Some(7),
        ..HttpLimits::default()
    })
    .await;
    let mut stream = TcpStream::connect(addr).await.unwrap();
    // Client preface plus an empty SETTINGS frame.
    stream
        .write_all(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n\x00\x00\x00\x04\x00\x00\x00\x00\x00")
        .await
        .unwrap();
    // The server's first frame is its SETTINGS frame.
    let mut header = [0_u8; 9];
    tokio::time::timeout(Duration::from_secs(5), stream.read_exact(&mut header))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(header[3], 0x4, "first server frame must be SETTINGS");
    let len = usize::from(header[0]) << 16 | usize::from(header[1]) << 8 | usize::from(header[2]);
    let mut payload = vec![0_u8; len];
    stream.read_exact(&mut payload).await.unwrap();
    let max_streams = payload.chunks(6).find_map(|setting| {
        (u16::from_be_bytes([setting[0], setting[1]]) == 0x3)
            .then(|| u32::from_be_bytes([setting[2], setting[3], setting[4], setting[5]]))
    });
    assert_eq!(max_streams, Some(7));
}

#[tokio::test]
async fn silent_client_is_closed_by_keep_alive_timeout_alone() {
    let addr = spawn_server(HttpLimits {
        keep_alive_timeout: ms(500),
        ..HttpLimits::default()
    })
    .await;
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let closed_after = time_until_closed(&mut stream, Duration::from_secs(5)).await;
    assert!(closed_after >= Duration::from_millis(300));
}

#[tokio::test]
async fn slow_header_client_cannot_hold_a_graceful_drain_open() {
    let router = Router::new().route("/", get(|| async { "ok" }));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(serve(
        listener,
        router.into_make_service(),
        HttpLimits {
            header_read_timeout: ms(500),
            ..HttpLimits::default()
        },
        async move {
            stop_rx.await.ok();
        },
    ));
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: test\r\n")
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    stop_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(3), server)
        .await
        .expect("drain must end once the slow head times out")
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn default_limits_keep_a_slow_header_connection_open() {
    let addr = spawn_server(HttpLimits::default()).await;
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(b"GET / HTTP/1.1\r\n").await.unwrap();
    let mut buf = [0_u8; 64];
    let read = tokio::time::timeout(Duration::from_millis(500), stream.read(&mut buf)).await;
    assert!(
        read.is_err(),
        "no limit set, so the connection must stay open"
    );
}

#[tokio::test]
async fn websocket_outlives_keep_alive_timeout() {
    use axum::extract::ws::{Message, WebSocketUpgrade};
    use futures::{SinkExt, StreamExt};

    let router = Router::new().route(
        "/ws",
        get(|ws: WebSocketUpgrade| async move {
            ws.on_upgrade(|mut socket| async move {
                while let Some(Ok(Message::Text(text))) = socket.recv().await {
                    if socket.send(Message::Text(text)).await.is_err() {
                        break;
                    }
                }
            })
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(serve(
        listener,
        router.into_make_service(),
        HttpLimits {
            header_read_timeout: ms(300),
            keep_alive_timeout: ms(300),
            ..HttpLimits::default()
        },
        std::future::pending(),
    ));
    let (mut client, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(900)).await;
    client
        .send(tokio_tungstenite::tungstenite::Message::text("alive"))
        .await
        .unwrap();
    let echo = tokio::time::timeout(Duration::from_secs(5), client.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(echo, tokio_tungstenite::tungstenite::Message::text("alive"));
}

#[test]
fn limits_resolve_from_config_with_zero_as_off() {
    let config = autumn_web::config::HttpServerConfig {
        header_read_timeout_ms: Some(0),
        keep_alive_timeout_ms: Some(1500),
        max_header_bytes: Some(16_384),
        http2_max_concurrent_streams: Some(4),
        max_connections: Some(0),
    };
    assert_eq!(
        HttpLimits::from(&config),
        HttpLimits {
            header_read_timeout: None,
            keep_alive_timeout: ms(1500),
            max_header_bytes: Some(16_384),
            http2_max_concurrent_streams: Some(4),
            max_connections: None,
        }
    );
}
