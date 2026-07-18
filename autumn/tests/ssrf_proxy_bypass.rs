//! Isolated integration test: pinned SSRF-safe clients must bypass environment
//! proxies (`HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY`).
//!
//! This lives in its own test binary (not the consolidated suite) because it
//! sets **process-wide** proxy environment variables. Those would race with the
//! inline `http_client` network tests if set in-process; a dedicated binary runs
//! in its own process, so the mutation is isolated. See CLAUDE.md
//! ("Isolated Integration Tests"). Env vars are managed through
//! `temp_env::async_with_vars` (the workspace forbids `unsafe_code`, so raw
//! `std::env::set_var` is unavailable).
//!
//! Regression coverage for the Codex P1 finding: reqwest evaluates proxy
//! interception BEFORE the connector where `ClientBuilder::resolve(host, addr)`
//! applies. Without `.no_proxy()` on the pinned client, a configured env proxy
//! would receive the request and re-resolve the target host, defeating the pin
//! (reopening the DNS-rebinding / SSRF window). We point every proxy env var at
//! a black-hole address and assert that a `pin_to` request STILL reaches its
//! pinned loopback listener — proving the dead proxy was bypassed.

#![cfg(feature = "http-client")]

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use autumn_web::http::Client;

/// `pin_to` connects directly to the pinned address even when `HTTP_PROXY` /
/// `HTTPS_PROXY` / `ALL_PROXY` point at a dead (black-hole) proxy. If the pinned
/// client honoured the proxy the request would be routed to `127.0.0.1:1`
/// (nothing listens there) and fail; success proves the proxy was bypassed.
#[tokio::test]
async fn pinned_request_bypasses_env_proxy() {
    use axum::{Router, routing::get};

    // Black-hole proxy: 127.0.0.1:1 has no listener, so any request that
    // honoured it would fail to connect.
    const DEAD_PROXY: &str = "http://127.0.0.1:1";

    // Loopback listener on an ephemeral port.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let port = addr.port();
    tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route("/ping", get(|| async { "pong" })),
        )
        .await
        .unwrap();
    });

    // The proxy env vars are only set for the duration of this future, then
    // restored by `temp_env` — keeping the process-wide mutation scoped.
    temp_env::async_with_vars(
        [
            ("HTTP_PROXY", Some(DEAD_PROXY)),
            ("HTTPS_PROXY", Some(DEAD_PROXY)),
            ("ALL_PROXY", Some(DEAD_PROXY)),
        ],
        async move {
            // `pin_to` trusts the caller-supplied addr (it does NOT run the SSRF
            // guard, so pinning to loopback is allowed). The host
            // `pinned.invalid` never resolves (.invalid TLD), so reaching the
            // listener proves DNS was bypassed via the pin — and, crucially,
            // that the request did NOT go through the dead proxy set above.
            let resp = Client::new()
                .get(format!("http://pinned.invalid:{port}/ping"))
                .pin_to(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port))
                .send()
                .await
                .expect("pinned request must bypass the dead env proxy and reach the listener");

            assert_eq!(resp.status().as_u16(), 200);
            assert_eq!(resp.text(), "pong");
        },
    )
    .await;
}
