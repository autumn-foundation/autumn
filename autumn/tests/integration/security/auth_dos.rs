use std::time::{Duration, Instant};

use autumn_web::auth::hash_password;
use axum::{Router, routing::get, routing::post};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn test_login_material() -> String {
    String::from_utf8(
        [
            97, 116, 116, 97, 99, 107, 101, 114, 95, 112, 97, 115, 115, 119, 111, 114, 100,
        ]
        .to_vec(),
    )
    .expect("test fixture bytes should be valid utf-8")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn eris_auth_dos_poc() {
    // A login route that does heavy bcrypt work.
    // In vulnerable code, this function runs synchronously,
    // blocking the tokio worker thread.
    async fn login_handler() -> &'static str {
        let credential = test_login_material();
        let _ = hash_password(&credential).await;
        "ok"
    }

    // A fast endpoint that does no heavy work.
    async fn ping_handler() -> &'static str {
        "pong"
    }

    let app = Router::new()
        .route("/login", post(login_handler))
        .route("/ping", get(ping_handler));

    // Start server in background
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    // We use a simple tcp connection and manually send the HTTP request
    // This avoids dependency issues in the tests module.

    // We spawn a lot of concurrent hashing tasks to ensure we block the pool.
    let mut login_tasks = vec![];
    for _ in 0..64 {
        login_tasks.push(tokio::spawn(async move {
            if let Ok(mut stream) = tokio::net::TcpStream::connect(addr).await {
                let req = b"POST /login HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\nContent-Length: 0\r\n\r\n";
                let _ = stream.write_all(req).await;
                // Wait for the response
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf).await;
            }
        }));
    }

    // Give the tasks enough time to start and saturate the handlers.
    // 1 second is sufficient for all 64 connections to be established and
    // for blocking work to hold worker threads (if the implementation is vulnerable).
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Now ping the server
    let ping_start = Instant::now();
    let mut ping_success = false;
    if let Ok(mut stream) = tokio::net::TcpStream::connect(addr).await {
        let req = b"GET /ping HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n";
        if stream.write_all(req).await.is_ok() {
            let mut buf = [0u8; 1024];
            if stream.read(&mut buf).await.is_ok() {
                ping_success = true;
            }
        }
    }

    let ping_duration = ping_start.elapsed();

    assert!(ping_success, "Ping request failed");

    // To prove the fix, we assert that ping returns in a reasonable time.
    // A true DoS (synchronous bcrypt blocking the async workers) would make
    // this take 30+ seconds, since every worker is held by a hash. The
    // non-blocking implementation (spawn_blocking) lets the async workers
    // service the ping even while hashes are in flight. The 2s threshold
    // leaves slack for constrained CI runners where 64 concurrent bcrypts
    // contend for CPU on the blocking threadpool.
    assert!(
        ping_duration < Duration::from_secs(2),
        "Ping took too long ({ping_duration:?}), indicating a Denial of Service via blocked worker threads!"
    );

    // cleanup
    for t in login_tasks {
        t.abort();
    }
}
