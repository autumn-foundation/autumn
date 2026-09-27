//! The ACME HTTP-01 challenge listener and HTTP→HTTPS redirect (issue #1608).
//!
//! When ACME is configured, the app binds a second listener on the challenge
//! port (default `:80`). It serves exactly two things:
//!
//! 1. `GET /.well-known/acme-challenge/{token}` — returns the key authorization
//!    the ACME CA expects, so the CA can validate domain control over plain
//!    HTTP. Tokens are published into [`Http01Tokens`] by the renewal task for
//!    the duration of an order and removed once the challenge is answered.
//! 2. Every other request — a `308 Permanent Redirect` to the `https://` origin,
//!    so a plain `http://` visitor is bounced to the TLS listener.
//!
//! The listener is a sibling of the main server under the same shutdown token,
//! so it tears down cleanly with the rest of the process.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::any;

/// A shared, mutable map of ACME HTTP-01 `token → key_authorization` entries.
///
/// The renewal task inserts an entry before telling the CA a challenge is ready
/// and removes it once the challenge is validated; the challenge router reads it
/// to answer `GET /.well-known/acme-challenge/{token}`.
#[derive(Clone, Default)]
pub struct Http01Tokens(Arc<RwLock<HashMap<String, String>>>);

impl Http01Tokens {
    /// Create an empty token map.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Publish a `token → key_authorization` entry.
    pub fn insert(&self, token: impl Into<String>, key_authorization: impl Into<String>) {
        self.write().insert(token.into(), key_authorization.into());
    }

    /// Look up the key authorization for `token`, if present.
    #[must_use]
    pub fn get(&self, token: &str) -> Option<String> {
        self.read().get(token).cloned()
    }

    /// Remove a published token (called once its challenge is answered).
    pub fn remove(&self, token: &str) {
        self.write().remove(token);
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, HashMap<String, String>> {
        self.0
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, HashMap<String, String>> {
        self.0
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Shared state for the challenge router.
#[derive(Clone)]
struct ChallengeState {
    tokens: Http01Tokens,
    https_port: u16,
}

/// Build the axum router served on the ACME challenge port.
///
/// - `GET /.well-known/acme-challenge/{token}` → `200 text/plain` with the key
///   authorization (or `404` if the token is unknown).
/// - Any other path/method → `308 Permanent Redirect` to the `https://` origin,
///   preserving the path and query. The `Host` header drives the redirect
///   target (with any `:80` stripped and `:{https_port}` appended when it is not
///   the default `443`); a request with no usable `Host` gets a `400`.
pub fn challenge_router(tokens: Http01Tokens, https_port: u16) -> Router {
    Router::new()
        .route(
            "/.well-known/acme-challenge/{token}",
            axum::routing::get(serve_challenge),
        )
        .fallback(any(redirect_to_https))
        .with_state(ChallengeState { tokens, https_port })
}

/// Bind the HTTP-01 challenge listener(s) so they accept BOTH IPv4 and IPv6.
///
/// Let's Encrypt validates HTTP-01 against whichever address family the domain's
/// A/AAAA records point at. An IPv4-only `0.0.0.0` bind leaves an AAAA-only
/// (IPv6-first) deployment unreachable on `:80`, so the CA gets connection
/// refused even while HTTPS is up.
///
/// Preferred: a single dual-stack `[::]:{port}` socket with `IPV6_V6ONLY = false`
/// (so IPv4 arrives as v4-mapped addresses) and `SO_REUSEADDR`. If the platform
/// refuses a dual-stack socket, fall back to binding `0.0.0.0:{port}` (the
/// mandatory IPv4 baseline) plus a best-effort v6-only `[::]:{port}` listener
/// when an IPv6 stack is present; the caller serves each. Returns at least one
/// listener on success.
///
/// # Errors
///
/// Returns the underlying [`std::io::Error`] only when the IPv4 baseline cannot
/// be bound (e.g. the port is privileged and the process lacks permission, or
/// the port is already in use), so the caller can fail fast — `:80` needs
/// privilege (`CAP_NET_BIND_SERVICE`) and ACME cannot validate without it. A
/// missing IPv6 stack is not fatal: an IPv4-only host still serves over IPv4.
pub async fn bind_challenge_listeners(port: u16) -> std::io::Result<Vec<tokio::net::TcpListener>> {
    // Preferred: one dual-stack `[::]` socket that also accepts IPv4 (v4-mapped).
    if let Ok(listener) = bind_ipv6(port, false) {
        return Ok(vec![listener]);
    }

    // Fallback: the platform refused a dual-stack socket. Bind IPv4 `0.0.0.0` as
    // the mandatory baseline (its failure is fatal), then ADD a v6-only `[::]`
    // listener when IPv6 is available — a missing IPv6 stack must not take down
    // an otherwise-serviceable IPv4-only deployment.
    tracing::warn!(
        port,
        "Dual-stack [::] bind for the ACME HTTP-01 challenge listener is unavailable; binding \
         IPv4 0.0.0.0 with a best-effort IPv6 [::] listener"
    );
    let v4 = tokio::net::TcpListener::bind((std::net::Ipv4Addr::UNSPECIFIED, port)).await?;
    let mut listeners = vec![v4];
    // v6-only so `[::]` does not also claim IPv4 and collide with the `0.0.0.0`
    // bind above (SO_REUSEADDR lets the two families share the port regardless).
    match bind_ipv6(port, true) {
        Ok(v6) => listeners.push(v6),
        Err(e) => tracing::warn!(
            error = %e,
            port,
            "IPv6 [::] bind for the ACME HTTP-01 challenge listener failed; serving IPv4 only"
        ),
    }
    Ok(listeners)
}

/// Bind an IPv6 (`[::]:{port}`) TCP listener with `SO_REUSEADDR`, controlling the
/// `IPV6_V6ONLY` flag: `only_v6 = false` yields a dual-stack socket that also
/// accepts IPv4 (v4-mapped); `only_v6 = true` restricts it to IPv6.
fn bind_ipv6(port: u16, only_v6: bool) -> std::io::Result<tokio::net::TcpListener> {
    use socket2::{Domain, Protocol, Socket, Type};

    let socket = Socket::new(Domain::IPV6, Type::STREAM, Some(Protocol::TCP))?;
    socket.set_only_v6(only_v6)?;
    socket.set_reuse_address(true)?;
    let addr: std::net::SocketAddr = (std::net::Ipv6Addr::UNSPECIFIED, port).into();
    socket.bind(&addr.into())?;
    // A generous backlog: the challenge listener answers short-lived HTTP-01
    // validation requests and HTTP→HTTPS redirects.
    socket.listen(1024)?;
    socket.set_nonblocking(true)?;
    tokio::net::TcpListener::from_std(socket.into())
}

/// Answer an HTTP-01 challenge: return the published key authorization, or 404.
async fn serve_challenge(
    State(state): State<ChallengeState>,
    Path(token): Path<String>,
) -> Response {
    state.tokens.get(&token).map_or_else(
        || (StatusCode::NOT_FOUND, "unknown ACME challenge token").into_response(),
        |key_authorization| {
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "text/plain")],
                key_authorization,
            )
                .into_response()
        },
    )
}

/// Redirect any non-challenge request to the HTTPS origin (308).
async fn redirect_to_https(
    State(state): State<ChallengeState>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let Some(location) = https_location(&headers, &uri, state.https_port) else {
        return (
            StatusCode::BAD_REQUEST,
            "missing or invalid Host header; cannot build HTTPS redirect",
        )
            .into_response();
    };
    header::HeaderValue::from_str(&location).map_or_else(
        |_| (StatusCode::BAD_REQUEST, "invalid Host header").into_response(),
        |value| (StatusCode::PERMANENT_REDIRECT, [(header::LOCATION, value)]).into_response(),
    )
}

/// Compute the `https://` redirect target from the request's `Host` header and
/// URI. Returns `None` when no usable `Host` is present.
///
/// The host's `:80` (or a bare trailing port) is stripped and `:{https_port}`
/// re-appended only when `https_port` is not the default `443`, so the common
/// case (`80 → 443`) yields a clean `https://host/path?query`.
fn https_location(headers: &HeaderMap, uri: &Uri, https_port: u16) -> Option<String> {
    let host_header = headers.get(header::HOST)?.to_str().ok()?;
    let host = host_only(host_header);
    if host.is_empty() {
        return None;
    }
    let path_and_query = uri
        .path_and_query()
        .map_or_else(|| uri.path().to_owned(), ToString::to_string);
    let authority = if https_port == 443 {
        host.to_owned()
    } else {
        format!("{host}:{https_port}")
    };
    Some(format!("https://{authority}{path_and_query}"))
}

/// Strip any `:port` suffix from a `Host` header value, leaving just the host.
///
/// Handles bracketed IPv6 literals (`[::1]:80`) by keeping the bracketed host.
fn host_only(host_header: &str) -> &str {
    if let Some(end) = host_header
        .strip_prefix('[')
        .and_then(|_| host_header.find(']'))
    {
        // `[ipv6]` or `[ipv6]:port` — keep through the closing bracket.
        return &host_header[..=end];
    }
    match host_header.rsplit_once(':') {
        Some((host, _port)) => host,
        None => host_header,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt as _; // for `oneshot`

    #[test]
    fn tokens_round_trip() {
        let tokens = Http01Tokens::new();
        assert_eq!(tokens.get("tok"), None);
        tokens.insert("tok", "tok.keyauth");
        assert_eq!(tokens.get("tok").as_deref(), Some("tok.keyauth"));
        tokens.remove("tok");
        assert_eq!(tokens.get("tok"), None);
    }

    fn body_to_string(body: Body) -> String {
        futures::executor::block_on(async {
            let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
            String::from_utf8(bytes.to_vec()).unwrap()
        })
    }

    #[tokio::test]
    async fn present_token_returns_key_authorization() {
        let tokens = Http01Tokens::new();
        tokens.insert("abc", "abc.keyauth");
        let app = challenge_router(tokens, 443);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/.well-known/acme-challenge/abc")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/plain"
        );
        assert_eq!(body_to_string(resp.into_body()), "abc.keyauth");
    }

    #[tokio::test]
    async fn absent_token_returns_404() {
        let app = challenge_router(Http01Tokens::new(), 443);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/.well-known/acme-challenge/missing")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn other_path_redirects_to_https_preserving_path_and_query() {
        let app = challenge_router(Http01Tokens::new(), 443);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/dashboard?tab=2")
                    .header(header::HOST, "app.example.com")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::PERMANENT_REDIRECT);
        assert_eq!(
            resp.headers().get(header::LOCATION).unwrap(),
            "https://app.example.com/dashboard?tab=2"
        );
    }

    #[tokio::test]
    async fn redirect_strips_port_80_and_appends_nondefault_https_port() {
        let app = challenge_router(Http01Tokens::new(), 8443);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/x")
                    .header(header::HOST, "app.example.com:80")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::PERMANENT_REDIRECT);
        assert_eq!(
            resp.headers().get(header::LOCATION).unwrap(),
            "https://app.example.com:8443/x"
        );
    }

    #[tokio::test]
    async fn missing_host_is_bad_request() {
        let app = challenge_router(Http01Tokens::new(), 443);
        let resp = app
            .oneshot(Request::builder().uri("/x").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn host_only_strips_port_and_keeps_ipv6() {
        assert_eq!(host_only("example.com"), "example.com");
        assert_eq!(host_only("example.com:80"), "example.com");
        assert_eq!(host_only("[::1]:80"), "[::1]");
        assert_eq!(host_only("[::1]"), "[::1]");
    }

    /// Serve `listeners` with the challenge router and assert an IPv4 loopback
    /// connection reaches it (a `308` redirect from the fallback route confirms
    /// the router actually served the connection).
    async fn assert_ipv4_served(listeners: Vec<tokio::net::TcpListener>) {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        assert!(!listeners.is_empty(), "must bind at least one listener");
        let port = listeners[0].local_addr().unwrap().port();

        let router = challenge_router(Http01Tokens::new(), 443);
        let mut serve_handles = Vec::new();
        for listener in listeners {
            let router = router.clone();
            serve_handles.push(tokio::spawn(async move {
                let _ = axum::serve(listener, router).await;
            }));
        }

        let mut stream = tokio::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port))
            .await
            .expect("IPv4 loopback connection to the challenge listener");
        stream
            .write_all(b"GET / HTTP/1.0\r\nHost: app.example.com\r\n\r\n")
            .await
            .unwrap();
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.unwrap();
        let response = String::from_utf8_lossy(&buf);
        assert!(
            response.contains("308"),
            "challenge router must serve the IPv4 connection (expected a 308 redirect), \
             got: {response}"
        );

        for handle in serve_handles {
            handle.abort();
        }
    }

    // Regression (#1608, Codex): the HTTP-01 challenge listener must accept BOTH
    // IPv4 and IPv6 so Let's Encrypt can validate an AAAA-only deployment.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn challenge_listener_accepts_ipv4_and_is_dual_stack_when_available() {
        // The app-facing entrypoint always yields an IPv4-reachable listener,
        // whether it took the dual-stack path or the IPv4 baseline fallback (an
        // IPv4-only host has no IPv6 stack for the `[::]` socket).
        let listeners = bind_challenge_listeners(0)
            .await
            .expect("bind ephemeral challenge listener");
        assert_ipv4_served(listeners).await;

        // Where the platform supports a dual-stack socket, an IPv4 connection
        // accepted on the IPv6-bound `[::]` socket is the definitive proof that
        // IPV6_V6ONLY=false (v4-mapped addresses are accepted). On an IPv4-only
        // host this bind fails, and the invariant is already covered above.
        if let Ok(dual) = bind_ipv6(0, false) {
            assert!(
                dual.local_addr().unwrap().is_ipv6(),
                "the dual-stack challenge listener must be bound on the IPv6 wildcard [::]"
            );
            assert_ipv4_served(vec![dual]).await;
        }
    }
}
