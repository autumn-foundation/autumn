//! Centralized trusted-proxy policy for forwarded-header middleware.
//!
//! This module provides [`ProxyResolver`] — the single source of truth for
//! evaluating `X-Forwarded-*` headers. Every framework middleware that needs
//! a "real" client IP, host, or scheme must go through this resolver instead
//! of reading forwarding headers directly.
//!
//! # Design
//!
//! Operators declare their proxy trust boundary once in `[security.trusted_proxies]`
//! and every middleware honours it automatically.  The resolver supports two
//! trust strategies:
//!
//! - **CIDR ranges** (`ranges`): walk from the right of the `X-Forwarded-For`
//!   chain, skipping IPs that fall inside a trusted range. The first untrusted
//!   IP is the real client.
//! - **Hop count** (`trusted_hops`): strip exactly N entries from the right of
//!   the chain. Useful when the exact proxy IPs are dynamic (e.g., ALB).
//!
//! When `trust_forwarded_headers = false` (the default in `prod` without config)
//! the resolver ignores all `X-Forwarded-*` regardless of the chain.
//!
//! # Profile-aware defaults
//!
//! | Profile | Default |
//! |---------|---------|
//! | `dev`   | Trust loopback only (`127.0.0.1/8`, `::1/128`) |
//! | `prod`  | No forwarding trust until configured |
//!
//! # Plugin authors
//!
//! > **Never read `X-Forwarded-*` directly. Use the `ClientAddr`, `ClientHost`,
//! > or `ClientScheme` extractors from `autumn_web::extract`.**
//!
//! These extractors are resolved by a framework-managed [`ProxyResolver`] and
//! are the only blessed way to obtain real client identity from request
//! handlers and middleware.

// autumn-panic-gate: request-path module — production code path must be panic-free.
// See CONTRIBUTING.md "Request-path panic gate". Justify exceptions with
// #[allow(clippy::<lint>, reason = "…")] at the narrowest scope.
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented,
        clippy::indexing_slicing,
    )
)]

use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::extract::ConnectInfo;
use axum::http::Request;
use tower::{Layer, Service};

use crate::security::config::TrustedProxiesConfig;

/// A parsed trusted-proxy CIDR range or exact IP.
#[derive(Debug, Clone, Copy)]
struct TrustedProxy {
    network: IpAddr,
    prefix_len: u8,
}

impl TrustedProxy {
    fn parse(value: &str) -> Option<Self> {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return None;
        }

        let (addr, prefix_len) = if let Some((addr, prefix)) = trimmed.split_once('/') {
            let addr = addr.trim().parse::<IpAddr>().ok()?;
            let prefix_len = prefix.trim().parse::<u8>().ok()?;
            (addr, prefix_len)
        } else {
            let addr = trimmed.parse::<IpAddr>().ok()?;
            let prefix_len = match addr {
                IpAddr::V4(_) => 32,
                IpAddr::V6(_) => 128,
            };
            (addr, prefix_len)
        };

        let max_prefix = match addr {
            IpAddr::V4(_) => 32,
            IpAddr::V6(_) => 128,
        };

        (prefix_len <= max_prefix).then_some(Self {
            network: addr,
            prefix_len,
        })
    }

    fn contains(&self, ip: IpAddr) -> bool {
        if self.prefix_len == 0 {
            return matches!(
                (self.network, ip),
                (IpAddr::V4(_), IpAddr::V4(_)) | (IpAddr::V6(_), IpAddr::V6(_))
            );
        }

        match (self.network, ip) {
            (IpAddr::V4(network), IpAddr::V4(candidate)) => {
                let shift = 32_u8.saturating_sub(self.prefix_len);
                (u32::from(network) >> shift) == (u32::from(candidate) >> shift)
            }
            (IpAddr::V6(network), IpAddr::V6(candidate)) => {
                let shift = 128_u8.saturating_sub(self.prefix_len);
                (u128::from(network) >> shift) == (u128::from(candidate) >> shift)
            }
            (IpAddr::V4(_), IpAddr::V6(_)) | (IpAddr::V6(_), IpAddr::V4(_)) => false,
        }
    }
}

/// Resolves real client identity from `X-Forwarded-*` headers, honoring the
/// operator-configured trusted-proxy policy.
///
/// Construct via [`ProxyResolver::from_config`] and call the `resolve_*`
/// methods inside middleware. Framework-wired middleware receive a shared
/// resolver; plugin authors should use the [`ClientAddr`], [`ClientHost`], and
/// [`ClientScheme`] extractors from `autumn_web::extract` instead of calling
/// this resolver directly.
///
/// [`ClientAddr`]: crate::extract::ClientAddr
/// [`ClientHost`]: crate::extract::ClientHost
/// [`ClientScheme`]: crate::extract::ClientScheme
#[derive(Debug, Clone)]
pub struct ProxyResolver {
    ranges: Vec<TrustedProxy>,
    /// True when the original config had at least one range entry (even if all
    /// failed to parse).  When `true` and `ranges` is empty (all entries were
    /// invalid), we treat NO peer as trusted rather than all peers as trusted.
    ranges_configured: bool,
    trusted_hops: Option<u32>,
    trust_forwarded_headers: bool,
}

impl ProxyResolver {
    /// Construct a resolver from the operator's `[security.trusted_proxies]` config block.
    #[must_use]
    pub fn from_config(config: &TrustedProxiesConfig) -> Self {
        let ranges_configured = !config.ranges.is_empty();
        let ranges = config
            .ranges
            .iter()
            .filter_map(|proxy| {
                TrustedProxy::parse(proxy).or_else(|| {
                    tracing::warn!(
                        range = %proxy,
                        "ignoring invalid trusted_proxies range"
                    );
                    None
                })
            })
            .collect();

        Self {
            ranges,
            ranges_configured,
            trusted_hops: config.trusted_hops,
            trust_forwarded_headers: config.trust_forwarded_headers,
        }
    }

    /// Returns `true` when `ip` is a trusted proxy per the configured ranges.
    ///
    /// When no ranges are configured (`ranges_configured = false`), returns
    /// `true` for any IP (trust all peers). When ranges were configured but all
    /// failed to parse, returns `false` for any IP (trust no peers).
    fn is_trusted_ip(&self, ip: IpAddr) -> bool {
        if !self.ranges_configured {
            return true;
        }
        self.ranges.iter().any(|r| r.contains(ip))
    }

    /// Build a resolver that trusts loopback addresses only (dev-profile default).
    #[must_use]
    pub fn loopback_only() -> Self {
        Self::from_config(&TrustedProxiesConfig {
            ranges: vec!["127.0.0.0/8".to_owned(), "::1/128".to_owned()],
            trusted_hops: None,
            trust_forwarded_headers: true,
        })
    }

    /// Build a resolver that trusts no forwarding headers (prod default when unconfigured).
    #[must_use]
    pub const fn no_trust() -> Self {
        Self {
            ranges: Vec::new(),
            ranges_configured: false,
            trusted_hops: None,
            trust_forwarded_headers: false,
        }
    }

    /// Resolve the real client IP address from the request.
    ///
    /// When `trust_forwarded_headers` is `false`, returns the TCP peer IP.
    /// When `trust_forwarded_headers` is `true`:
    ///   - If `trusted_hops` is set, peels exactly that many entries from the
    ///     right of the `X-Forwarded-For` chain.
    ///   - If CIDR ranges are configured, walks from the right skipping trusted
    ///     IPs and returns the first untrusted IP.
    ///   - If neither is configured, falls back to the rightmost entry
    ///     (single-proxy assumption), then `X-Real-IP`, then peer IP.
    pub fn resolve_client_addr<B>(&self, req: &Request<B>) -> Option<IpAddr> {
        let peer_ip = Self::peer_ip(req);

        if !self.trust_forwarded_headers {
            return peer_ip;
        }

        // Hop-count mode: peel exactly N entries from the right regardless of
        // peer IP ranges.  The operator explicitly declared the number of proxy
        // hops to skip, so no range membership check is needed (and would break
        // mixed topologies like a dynamic ALB peer plus CDN ranges in the XFF
        // chain).  parse_forwarded_ip normalises entries that carry a port suffix.
        if let Some(hops) = self.trusted_hops {
            if let Some(xff) = Self::x_forwarded_for(req) {
                let mut entries = xff
                    .rsplit(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .filter_map(Self::parse_forwarded_ip);

                if let Some(ip) = entries.nth(hops as usize) {
                    return Some(ip);
                }
            }
            return peer_ip;
        }

        // Range-based resolution: honour forwarding headers only when the
        // immediate peer is trusted.  "No ranges configured" means trust all
        // peers.  "Ranges configured but all invalid" means trust no peers.
        let peer_is_trusted = peer_ip.is_some_and(|ip| self.is_trusted_ip(ip))
            || (!self.ranges_configured && peer_ip.is_none());

        if !peer_is_trusted {
            return peer_ip;
        }

        if let Some(xff) = Self::x_forwarded_for(req) {
            if self.ranges_configured {
                for entry in xff.rsplit(',').map(str::trim).filter(|s| !s.is_empty()) {
                    let Some(ip) = Self::parse_forwarded_ip(entry) else {
                        continue;
                    };
                    if !self.ranges.iter().any(|r| r.contains(ip)) {
                        return Some(ip);
                    }
                }
                return peer_ip;
            }

            // No ranges, no hop count: use rightmost entry, but skip if it equals
            // the peer (i.e. the proxy appended itself).
            let mut entries = xff
                .rsplit(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .filter_map(Self::parse_forwarded_ip);
            if let Some(rightmost_ip) = entries.next() {
                if peer_ip.is_some_and(|p| rightmost_ip == p) {
                    if let Some(prev_ip) = entries.next() {
                        return Some(prev_ip);
                    }
                    return Some(rightmost_ip);
                }
                return Some(rightmost_ip);
            }
        }

        // XFF absent or empty — fall through to X-Real-IP, then peer IP.
        if let Some(real_ip) = req
            .headers()
            .get("x-real-ip")
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .and_then(|s| s.parse::<IpAddr>().ok())
        {
            return Some(real_ip);
        }

        peer_ip
    }

    /// Resolve the external host as seen by the client.
    ///
    /// Returns the leftmost `X-Forwarded-Host` value when
    /// `trust_forwarded_headers` is `true` and the peer is trusted; falls back
    /// to the `Host` header.  In hop-count mode the peer is considered trusted
    /// only when the XFF chain contains enough valid-IP entries to satisfy the
    /// declared hop count — an absent or junk-filled chain cannot be used to
    /// spoof the host.
    pub fn resolve_client_host<B>(&self, req: &Request<B>) -> Option<String> {
        if !self.trust_forwarded_headers {
            return req
                .headers()
                .get(axum::http::header::HOST)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.trim().to_owned());
        }

        let peer_ip = Self::peer_ip(req);
        // Hop-count mode: only trust the forwarded host when the XFF chain
        // contains enough parseable IP entries.  A direct client or a proxy
        // that omits XFF cannot satisfy the count, preventing header spoofing.
        let peer_is_trusted = self.trusted_hops.map_or_else(
            || peer_ip.is_some_and(|ip| self.is_trusted_ip(ip)) || !self.ranges_configured,
            |hops| {
                Self::x_forwarded_for(req).is_some_and(|xff| {
                    xff.rsplit(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .filter_map(Self::parse_forwarded_ip)
                        .count()
                        > hops as usize
                })
            },
        );

        if peer_is_trusted
            && let Some(fwd_host_raw) = req
                .headers()
                .get("x-forwarded-host")
                .and_then(|v| v.to_str().ok())
        {
            // Multiple proxies may comma-append; the leftmost is the client-facing host.
            let host = fwd_host_raw
                .split(',')
                .next()
                .unwrap_or(fwd_host_raw)
                .trim();
            if !host.is_empty() {
                return Some(host.to_owned());
            }
        }

        req.headers()
            .get(axum::http::header::HOST)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.trim().to_owned())
    }

    /// Resolve the external scheme (`"http"` or `"https"`) as seen by the client.
    ///
    /// Returns the leftmost value of `X-Forwarded-Proto` when
    /// `trust_forwarded_headers` is `true` and the peer is trusted; otherwise
    /// the peer is considered trusted only when the XFF chain contains enough
    /// valid-IP entries — matching the same guard used by `resolve_client_host`.
    /// Returns `None` when no scheme signal is available, letting callers
    /// distinguish "no signal" from `"http"` (important for the same-origin
    /// host-only fallback in method-override).
    pub fn resolve_client_scheme<B>(&self, req: &Request<B>) -> Option<String> {
        if self.trust_forwarded_headers {
            let peer_ip = Self::peer_ip(req);
            let peer_is_trusted = self.trusted_hops.map_or_else(
                || peer_ip.is_some_and(|ip| self.is_trusted_ip(ip)) || !self.ranges_configured,
                |hops| {
                    Self::x_forwarded_for(req).is_some_and(|xff| {
                        xff.rsplit(',')
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .filter_map(Self::parse_forwarded_ip)
                            .count()
                            > hops as usize
                    })
                },
            );

            if peer_is_trusted
                && let Some(proto) = req
                    .headers()
                    .get("x-forwarded-proto")
                    .and_then(|v| v.to_str().ok())
            {
                // Multiple values are comma-separated; the leftmost is the
                // client-facing scheme.
                let outermost = proto.split(',').next().unwrap_or(proto).trim();
                if !outermost.is_empty() {
                    return Some(outermost.to_ascii_lowercase());
                }
            }
        }

        req.uri().scheme_str().map(ToOwned::to_owned)
    }

    /// Parse an XFF entry that may carry an optional port suffix.
    ///
    /// Handles the three forms proxies are known to emit:
    /// - `"198.51.100.7"` (plain IPv4)
    /// - `"198.51.100.7:54321"` (IPv4 with port)
    /// - `"2001:db8::1"` (plain IPv6)
    /// - `"[2001:db8::1]:54321"` (bracketed IPv6 with port)
    fn parse_forwarded_ip(s: &str) -> Option<IpAddr> {
        s.parse::<IpAddr>()
            .ok()
            .or_else(|| s.parse::<SocketAddr>().map(|sa| sa.ip()).ok())
    }

    fn peer_ip<B>(req: &Request<B>) -> Option<IpAddr> {
        req.extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .map(|ConnectInfo(addr)| addr.ip())
    }

    fn x_forwarded_for<B>(req: &Request<B>) -> Option<String> {
        let all: Vec<&str> = req
            .headers()
            .get_all("x-forwarded-for")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .collect();

        if all.is_empty() {
            None
        } else {
            Some(all.join(", "))
        }
    }
}

// ── Fuzzing seams ───────────────────────────────────────────────────────────
// These `#[cfg(fuzzing)]` wrappers expose the private trusted-proxy parsing
// surface (`TrustedProxy::parse`, `ProxyResolver::parse_forwarded_ip`, and the
// full `X-Forwarded-*` resolver) to the `fuzz/` crate. They are compiled out of
// every normal build (including `cargo package`), so the published crate is
// byte-identical. See `fuzz/fuzz_targets/headers.rs`.

/// Fuzzing seam: parse a single `[security.trusted_proxies]` range/IP entry.
#[cfg(fuzzing)]
pub fn __fuzz_parse_trusted_proxy(value: &str) -> bool {
    TrustedProxy::parse(value).is_some()
}

/// Fuzzing seam: parse a single `X-Forwarded-For` entry into an IP.
#[cfg(fuzzing)]
#[must_use]
pub fn __fuzz_parse_forwarded_ip(value: &str) -> Option<IpAddr> {
    ProxyResolver::parse_forwarded_ip(value)
}

/// Fuzzing seam: run the full trusted-proxy resolver over arbitrary
/// `X-Forwarded-*` / `X-Real-IP` / `Host` header values.
#[cfg(fuzzing)]
pub fn __fuzz_resolve_forwarded(
    xff: &str,
    x_real_ip: &str,
    x_forwarded_proto: &str,
    x_forwarded_host: &str,
    host: &str,
) {
    use axum::http::HeaderValue;
    let resolver = ProxyResolver::loopback_only();
    let mut req = Request::new(());
    // Give the request a loopback peer so the trusted path is exercised.
    req.extensions_mut()
        .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 8080))));
    let headers = req.headers_mut();
    for (name, value) in [
        ("x-forwarded-for", xff),
        ("x-real-ip", x_real_ip),
        ("x-forwarded-proto", x_forwarded_proto),
        ("x-forwarded-host", x_forwarded_host),
        ("host", host),
    ] {
        if let Ok(v) = HeaderValue::from_str(value) {
            headers.insert(name, v);
        }
    }
    let _ = resolver.resolve_client_addr(&req);
    let _ = resolver.resolve_client_host(&req);
    let _ = resolver.resolve_client_scheme(&req);
}

/// Resolved client identity, injected into request extensions by the
/// framework's proxy-resolver middleware.  Extractors read from this.
#[derive(Debug, Clone)]
pub struct ResolvedClientIdentity {
    /// Resolved client IP (after trust evaluation).
    pub addr: Option<IpAddr>,
    /// Resolved external host.
    pub host: Option<String>,
    /// Resolved external scheme (`"http"` or `"https"`), or `None` when no
    /// scheme signal was observed (no trusted `X-Forwarded-Proto` and no URI scheme).
    /// Callers that need a concrete default should use
    /// `scheme.as_deref().unwrap_or("http")`.
    pub scheme: Option<String>,
}

/// Tower [`Layer`] that resolves real client identity and stamps
/// [`ResolvedClientIdentity`] into request extensions.
///
/// Install this early in the middleware stack — before rate limiting, CSRF,
/// and any middleware that reads the `ClientAddr`, `ClientHost`, or
/// `ClientScheme` extractors.
#[derive(Clone, Debug)]
pub struct TrustedProxiesLayer {
    resolver: Arc<ProxyResolver>,
}

impl TrustedProxiesLayer {
    /// Build from the operator's `[security.trusted_proxies]` config block.
    #[must_use]
    pub fn from_config(config: &TrustedProxiesConfig) -> Self {
        Self {
            resolver: Arc::new(ProxyResolver::from_config(config)),
        }
    }
}

impl<S> Layer<S> for TrustedProxiesLayer {
    type Service = TrustedProxiesService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        TrustedProxiesService {
            inner,
            resolver: Arc::clone(&self.resolver),
        }
    }
}

/// Tower [`Service`] produced by [`TrustedProxiesLayer`].
#[derive(Clone, Debug)]
pub struct TrustedProxiesService<S> {
    inner: S,
    resolver: Arc<ProxyResolver>,
}

impl<S, B> Service<Request<B>> for TrustedProxiesService<S>
where
    S: Service<Request<B>> + Clone + Send + 'static,
    S::Future: Send + 'static,
    S::Error: Send + 'static,
    B: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: Request<B>) -> Self::Future {
        let identity = ResolvedClientIdentity {
            addr: self.resolver.resolve_client_addr(&req),
            host: self.resolver.resolve_client_host(&req),
            scheme: self.resolver.resolve_client_scheme(&req),
        };
        req.extensions_mut().insert(identity);

        let mut inner = self.inner.clone();
        std::mem::swap(&mut self.inner, &mut inner);

        Box::pin(inner.call(req))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;

    fn req_with_xff(xff: &str) -> Request<()> {
        Request::builder()
            .header("x-forwarded-for", xff)
            .body(())
            .unwrap()
    }

    fn req_with_peer_and_xff(peer: &str, xff: &str) -> Request<()> {
        let addr: SocketAddr = format!("{peer}:1234").parse().unwrap();
        let mut req = Request::builder()
            .header("x-forwarded-for", xff)
            .body(())
            .unwrap();
        req.extensions_mut().insert(ConnectInfo(addr));
        req
    }

    // ── TrustedProxy parsing ───────────────────────────────────────────────

    #[test]
    fn trusted_proxy_parse_exact_ipv4() {
        let p = TrustedProxy::parse("10.0.0.1").unwrap();
        assert!(p.contains("10.0.0.1".parse().unwrap()));
        assert!(!p.contains("10.0.0.2".parse().unwrap()));
    }

    #[test]
    fn trusted_proxy_parse_cidr() {
        let p = TrustedProxy::parse("10.0.0.0/24").unwrap();
        assert!(p.contains("10.0.0.1".parse().unwrap()));
        assert!(p.contains("10.0.0.254".parse().unwrap()));
        assert!(!p.contains("10.0.1.0".parse().unwrap()));
    }

    #[test]
    fn trusted_proxy_parse_invalid_returns_none() {
        assert!(TrustedProxy::parse("not-an-ip").is_none());
        assert!(TrustedProxy::parse("").is_none());
        assert!(TrustedProxy::parse("10.0.0.0/33").is_none());
    }

    #[test]
    fn trusted_proxy_contains_ipv6() {
        let proxy = TrustedProxy::parse("2001:db8::/32").unwrap();
        assert!(proxy.contains("2001:db8:1234::1".parse().unwrap()));
        assert!(!proxy.contains("2001:db9::1".parse().unwrap()));

        let proxy_exact = TrustedProxy::parse("2001:db8::1").unwrap();
        assert!(proxy_exact.contains("2001:db8::1".parse().unwrap()));
        assert!(!proxy_exact.contains("2001:db8::2".parse().unwrap()));
    }

    // ── AC (a): attacker-controlled leading value rejected with trusted_hops=1 ──

    #[test]
    fn trusted_hops_one_rejects_attacker_controlled_leading_value() {
        // X-Forwarded-For: <attacker-injected>, <real-client>, <alb>
        // With trusted_hops=1 we peel 1 entry from the right (the ALB).
        // The real client is at position 1, NOT the attacker-injected value at 0.
        let resolver = ProxyResolver::from_config(&TrustedProxiesConfig {
            ranges: Vec::new(),
            trusted_hops: Some(1),
            trust_forwarded_headers: true,
        });

        let req = req_with_xff("1.2.3.4, 5.6.7.8, 10.0.0.1");
        // Peel 1 from right (10.0.0.1). Next is 5.6.7.8 (real client).
        // Attacker-injected 1.2.3.4 is NOT returned.
        let addr = resolver.resolve_client_addr(&req).unwrap();
        assert_eq!(addr, "5.6.7.8".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn trusted_hops_zero_uses_rightmost_entry() {
        let resolver = ProxyResolver::from_config(&TrustedProxiesConfig {
            ranges: Vec::new(),
            trusted_hops: Some(0),
            trust_forwarded_headers: true,
        });

        let req = req_with_xff("1.2.3.4, 5.6.7.8");
        let addr = resolver.resolve_client_addr(&req).unwrap();
        assert_eq!(addr, "5.6.7.8".parse::<IpAddr>().unwrap());
    }

    // ── AC (a2): hop-count with ranges list works even when peer not in ranges ─

    #[test]
    fn trusted_hops_with_ranges_does_not_require_peer_in_ranges() {
        // Dynamic ALB (10.0.1.200) is NOT in the CDN CIDR range, but trusted_hops
        // should bypass the peer-range check and peel exactly 1 hop regardless.
        let resolver = ProxyResolver::from_config(&TrustedProxiesConfig {
            ranges: vec!["203.0.113.0/24".to_owned()],
            trusted_hops: Some(1),
            trust_forwarded_headers: true,
        });

        // Peer is 10.0.1.200 (dynamic ALB, not in ranges). XFF: real-client, ALB.
        let req = req_with_peer_and_xff("10.0.1.200", "192.0.2.1, 10.0.1.200");
        // Peel 1 from right (10.0.1.200). The next entry is the real client.
        let addr = resolver.resolve_client_addr(&req).unwrap();
        assert_eq!(addr, "192.0.2.1".parse::<IpAddr>().unwrap());
    }

    // ── AC (b): two-hop CDN + ALB correctly identifies real client ────────────

    #[test]
    fn two_hop_cdn_alb_chain_identifies_real_client() {
        // CDN (203.0.113.10) -> ALB (10.0.1.100) -> app
        // X-Forwarded-For: <real-client>, 203.0.113.10, 10.0.1.100
        // Trusted ranges: CDN range + ALB range
        let resolver = ProxyResolver::from_config(&TrustedProxiesConfig {
            ranges: vec!["203.0.113.0/24".to_owned(), "10.0.0.0/8".to_owned()],
            trusted_hops: None,
            trust_forwarded_headers: true,
        });

        // Peer is the ALB (the immediate upstream of the app)
        let req = req_with_peer_and_xff("10.0.1.100", "192.0.2.1, 203.0.113.10, 10.0.1.100");
        let addr = resolver.resolve_client_addr(&req).unwrap();
        assert_eq!(
            addr,
            "192.0.2.1".parse::<IpAddr>().unwrap(),
            "real client must be identified correctly in a two-hop CDN+ALB chain"
        );
    }

    // ── AC (c): trust_forwarded_headers=false ignores all X-Forwarded-* ──────

    #[test]
    fn trust_forwarded_headers_false_ignores_xff() {
        let resolver = ProxyResolver::from_config(&TrustedProxiesConfig {
            ranges: vec!["10.0.0.0/8".to_owned()],
            trusted_hops: None,
            trust_forwarded_headers: false,
        });

        // Peer is 10.0.0.1 (trusted range), but trust_forwarded_headers=false
        // so the XFF chain must be ignored; we return the peer IP.
        let req = req_with_peer_and_xff("10.0.0.1", "192.0.2.1, 203.0.113.10");
        let addr = resolver.resolve_client_addr(&req).unwrap();
        assert_eq!(
            addr,
            "10.0.0.1".parse::<IpAddr>().unwrap(),
            "trust_forwarded_headers=false must ignore X-Forwarded-For"
        );
    }

    #[test]
    fn trust_forwarded_headers_false_ignores_x_forwarded_host() {
        let resolver = ProxyResolver::from_config(&TrustedProxiesConfig {
            ranges: Vec::new(),
            trusted_hops: None,
            trust_forwarded_headers: false,
        });

        let mut req = Request::builder()
            .header("host", "real.example")
            .header("x-forwarded-host", "attacker.example")
            .body(())
            .unwrap();
        req.extensions_mut()
            .insert(ConnectInfo("127.0.0.1:1234".parse::<SocketAddr>().unwrap()));

        let host = resolver.resolve_client_host(&req).unwrap();
        assert_eq!(host, "real.example");
    }

    #[test]
    fn trust_forwarded_headers_false_ignores_x_forwarded_proto() {
        let resolver = ProxyResolver::from_config(&TrustedProxiesConfig {
            ranges: Vec::new(),
            trusted_hops: None,
            trust_forwarded_headers: false,
        });

        let req = Request::builder()
            .header("x-forwarded-proto", "https")
            .uri("http://example.com/")
            .body(())
            .unwrap();

        let scheme = resolver.resolve_client_scheme(&req);
        assert_eq!(scheme.as_deref(), Some("http"));
    }

    // ── Host and scheme resolution ──────────────────────────────────────────

    #[test]
    fn resolve_scheme_from_forwarded_proto_leftmost() {
        let resolver = ProxyResolver::from_config(&TrustedProxiesConfig {
            ranges: Vec::new(),
            trusted_hops: None,
            trust_forwarded_headers: true,
        });

        // Multiple proxies — leftmost (client-facing) is https
        let req = Request::builder()
            .header("x-forwarded-proto", "https, http")
            .body(())
            .unwrap();
        assert_eq!(
            resolver.resolve_client_scheme(&req).as_deref(),
            Some("https")
        );
    }

    #[test]
    fn resolve_host_prefers_forwarded_host_when_trusted() {
        let resolver = ProxyResolver::from_config(&TrustedProxiesConfig {
            ranges: Vec::new(),
            trusted_hops: None,
            trust_forwarded_headers: true,
        });

        let req = Request::builder()
            .header("host", "internal.cluster.local")
            .header("x-forwarded-host", "public.example.com")
            .body(())
            .unwrap();
        let host = resolver.resolve_client_host(&req).unwrap();
        assert_eq!(host, "public.example.com");
    }

    #[test]
    fn resolve_host_returns_leftmost_when_chained() {
        // Proxy chain appended to X-Forwarded-Host; leftmost is client-facing.
        let resolver = ProxyResolver::from_config(&TrustedProxiesConfig {
            ranges: Vec::new(),
            trusted_hops: None,
            trust_forwarded_headers: true,
        });

        let req = Request::builder()
            .header(
                "x-forwarded-host",
                "public.example.com, internal.cluster.local",
            )
            .body(())
            .unwrap();
        let host = resolver.resolve_client_host(&req).unwrap();
        assert_eq!(host, "public.example.com");
    }

    #[test]
    fn resolve_host_falls_back_to_host_header() {
        let resolver = ProxyResolver::from_config(&TrustedProxiesConfig {
            ranges: Vec::new(),
            trusted_hops: None,
            trust_forwarded_headers: true,
        });

        let req = Request::builder()
            .header("host", "app.example.com")
            .body(())
            .unwrap();
        let host = resolver.resolve_client_host(&req).unwrap();
        assert_eq!(host, "app.example.com");
    }

    // ── Hop-count with ranges: host/scheme use forwarded headers too ──────────

    #[test]
    fn trusted_hops_with_ranges_resolves_forwarded_host() {
        // Dynamic ALB peer (10.0.1.200) not in CDN ranges; hop-count should still
        // trust X-Forwarded-Host when XFF chain has enough entries.
        // trusted_hops=1 requires count > 1 (i.e., at least 2 XFF entries).
        let resolver = ProxyResolver::from_config(&TrustedProxiesConfig {
            ranges: vec!["203.0.113.0/24".to_owned()],
            trusted_hops: Some(1),
            trust_forwarded_headers: true,
        });

        let mut req = Request::builder()
            .header("host", "internal.cluster.local")
            .header("x-forwarded-host", "public.example.com")
            // XFF: real-client, ALB — 2 entries satisfies count > 1
            .header("x-forwarded-for", "192.0.2.1, 10.0.1.200")
            .body(())
            .unwrap();
        let addr: SocketAddr = "10.0.1.200:1234".parse().unwrap();
        req.extensions_mut().insert(ConnectInfo(addr));

        let host = resolver.resolve_client_host(&req).unwrap();
        assert_eq!(host, "public.example.com");
    }

    #[test]
    fn trusted_hops_with_no_xff_does_not_trust_forwarded_host() {
        // When XFF chain is absent, a direct client could inject X-Forwarded-Host.
        // The resolver must fall back to the Host header.
        let resolver = ProxyResolver::from_config(&TrustedProxiesConfig {
            ranges: vec!["203.0.113.0/24".to_owned()],
            trusted_hops: Some(1),
            trust_forwarded_headers: true,
        });

        let mut req = Request::builder()
            .header("host", "real.example.com")
            .header("x-forwarded-host", "attacker.example.com")
            .body(())
            .unwrap();
        let addr: SocketAddr = "10.0.1.200:1234".parse().unwrap();
        req.extensions_mut().insert(ConnectInfo(addr));

        let host = resolver.resolve_client_host(&req).unwrap();
        assert_eq!(host, "real.example.com");
    }

    #[test]
    fn trusted_hops_with_ranges_resolves_forwarded_scheme() {
        let resolver = ProxyResolver::from_config(&TrustedProxiesConfig {
            ranges: vec!["203.0.113.0/24".to_owned()],
            trusted_hops: Some(1),
            trust_forwarded_headers: true,
        });

        let mut req = Request::builder()
            .header("x-forwarded-proto", "https")
            // XFF: real-client, ALB — 2 entries satisfies count > 1
            .header("x-forwarded-for", "192.0.2.1, 10.0.1.200")
            .body(())
            .unwrap();
        let addr: SocketAddr = "10.0.1.200:1234".parse().unwrap();
        req.extensions_mut().insert(ConnectInfo(addr));

        assert_eq!(
            resolver.resolve_client_scheme(&req).as_deref(),
            Some("https")
        );
    }

    #[test]
    fn trusted_hops_with_no_xff_does_not_trust_forwarded_scheme() {
        // Without an XFF chain, X-Forwarded-Proto must not be trusted.
        let resolver = ProxyResolver::from_config(&TrustedProxiesConfig {
            ranges: vec!["203.0.113.0/24".to_owned()],
            trusted_hops: Some(1),
            trust_forwarded_headers: true,
        });

        let req = Request::builder()
            .header("x-forwarded-proto", "https")
            .uri("http://example.com/")
            .body(())
            .unwrap();

        assert_eq!(
            resolver.resolve_client_scheme(&req).as_deref(),
            Some("http")
        );
    }

    // ── Untrusted peer ignores forwarding headers ───────────────────────────

    #[test]
    fn untrusted_peer_ignores_forwarding_headers() {
        let resolver = ProxyResolver::from_config(&TrustedProxiesConfig {
            ranges: vec!["10.0.0.0/8".to_owned()],
            trusted_hops: None,
            trust_forwarded_headers: true,
        });

        // Peer is 203.0.113.1 which is NOT in trusted ranges.
        let req = req_with_peer_and_xff("203.0.113.1", "192.0.2.1");
        // Must return the peer IP, not the XFF value.
        let addr = resolver.resolve_client_addr(&req).unwrap();
        assert_eq!(addr, "203.0.113.1".parse::<IpAddr>().unwrap());
    }

    // ── No peer IP (e.g. in tests without ConnectInfo) ─────────────────────

    #[test]
    fn no_peer_ip_with_trust_enabled_falls_back_to_xff() {
        // When no ConnectInfo (no peer known), and ranges is empty (trust all peers),
        // use XFF.
        let resolver = ProxyResolver::from_config(&TrustedProxiesConfig {
            ranges: Vec::new(),
            trusted_hops: None,
            trust_forwarded_headers: true,
        });

        let req = req_with_xff("192.0.2.1, 10.0.0.1");
        // No ConnectInfo; no peer to skip; no ranges configured -> trust any peer.
        // Rightmost is 10.0.0.1.
        let addr = resolver.resolve_client_addr(&req);
        assert!(addr.is_some());
    }

    // ── loopback_only() and no_trust() ─────────────────────────────────────

    #[test]
    fn loopback_only_trusts_loopback_xff() {
        let resolver = ProxyResolver::loopback_only();
        let req = req_with_peer_and_xff("127.0.0.1", "192.0.2.1");
        // Peer is loopback (trusted), so XFF is honoured.
        let addr = resolver.resolve_client_addr(&req).unwrap();
        assert_eq!(addr, "192.0.2.1".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn loopback_only_ignores_xff_from_non_loopback_peer() {
        let resolver = ProxyResolver::loopback_only();
        let req = req_with_peer_and_xff("10.0.0.1", "192.0.2.1");
        // Peer is NOT loopback; XFF must be ignored.
        let addr = resolver.resolve_client_addr(&req).unwrap();
        assert_eq!(addr, "10.0.0.1".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn no_trust_always_returns_peer_ip() {
        let resolver = ProxyResolver::no_trust();
        let req = req_with_peer_and_xff("10.0.0.1", "192.0.2.1");
        let addr = resolver.resolve_client_addr(&req).unwrap();
        assert_eq!(addr, "10.0.0.1".parse::<IpAddr>().unwrap());
    }

    // ── Port-normalisation in XFF entries ──────────────────────────────────

    #[test]
    fn xff_entry_with_ipv4_port_is_parsed() {
        let resolver = ProxyResolver::from_config(&TrustedProxiesConfig {
            ranges: Vec::new(),
            trusted_hops: Some(1),
            trust_forwarded_headers: true,
        });

        // Proxy emitted "client:port, alb:port" — both carry port suffixes.
        let req = req_with_xff("192.0.2.1:54321, 10.0.1.200:80");
        let addr = resolver.resolve_client_addr(&req).unwrap();
        assert_eq!(addr, "192.0.2.1".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn xff_entry_with_ipv6_port_is_parsed() {
        let resolver = ProxyResolver::from_config(&TrustedProxiesConfig {
            ranges: Vec::new(),
            trusted_hops: Some(1),
            trust_forwarded_headers: true,
        });

        // Bracketed IPv6 with port on ALB side.
        let req = req_with_xff("2001:db8::1, [::1]:8080");
        let addr = resolver.resolve_client_addr(&req).unwrap();
        assert_eq!(addr, "2001:db8::1".parse::<IpAddr>().unwrap());
    }

    // ── Junk XFF entries must not satisfy hop count ─────────────────────────

    #[test]
    fn trusted_hops_with_junk_xff_does_not_trust_forwarded_host() {
        // Non-IP XFF entries must not count toward the hop threshold.
        let resolver = ProxyResolver::from_config(&TrustedProxiesConfig {
            ranges: Vec::new(),
            trusted_hops: Some(1),
            trust_forwarded_headers: true,
        });

        let mut req = Request::builder()
            .header("host", "real.example.com")
            .header("x-forwarded-host", "attacker.example.com")
            // Two entries, but both are junk — count of valid IPs is 0, not > 1.
            .header("x-forwarded-for", "junk, more-junk")
            .body(())
            .unwrap();
        let addr: SocketAddr = "10.0.1.200:1234".parse().unwrap();
        req.extensions_mut().insert(ConnectInfo(addr));

        let host = resolver.resolve_client_host(&req).unwrap();
        assert_eq!(host, "real.example.com");
    }
}

#[cfg(test)]
mod proptests {
    //! Property-based invariants for the private trusted-proxy parsers.
    //! `TrustedProxy::parse` and `ProxyResolver::parse_forwarded_ip` are
    //! private, so they are fuzzed in-crate here rather than from an
    //! integration test.
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        /// `TrustedProxy::parse` never panics on arbitrary input.
        #[test]
        fn parse_range_never_panics(s in ".*") {
            let _ = TrustedProxy::parse(&s);
        }

        /// A bare valid IP parses, and (per the CIDR contract) `contains`
        /// treats it as trusting exactly itself among same-family addresses.
        #[test]
        fn parse_bare_ipv4_round_trips(a in any::<u8>(), b in any::<u8>(), c in any::<u8>(), d in any::<u8>()) {
            let ip = std::net::Ipv4Addr::new(a, b, c, d);
            let parsed = TrustedProxy::parse(&ip.to_string());
            prop_assert!(parsed.is_some());
            let range = parsed.unwrap();
            prop_assert!(range.contains(std::net::IpAddr::V4(ip)));
        }

        /// Surrounding whitespace is trimmed, so a padded IP parses to the same
        /// range as the unpadded one (normalisation idempotence at the parse
        /// boundary).
        #[test]
        fn parse_is_whitespace_insensitive(
            a in any::<u8>(), b in any::<u8>(), c in any::<u8>(), d in any::<u8>(),
            pad in "[ \t]{0,4}",
        ) {
            let ip = std::net::Ipv4Addr::new(a, b, c, d).to_string();
            let padded = format!("{pad}{ip}{pad}");
            let bare = TrustedProxy::parse(&ip);
            let padded_parsed = TrustedProxy::parse(&padded);
            prop_assert_eq!(bare.is_some(), padded_parsed.is_some());
        }

        /// A prefix length wider than the address family's maximum is rejected
        /// (never a panic, never an over-wide accept).
        #[test]
        fn parse_rejects_over_wide_prefix(a in any::<u8>(), b in any::<u8>(), c in any::<u8>(), d in any::<u8>(), prefix in 33u16..=255) {
            let s = format!("{a}.{b}.{c}.{d}/{prefix}");
            prop_assert!(TrustedProxy::parse(&s).is_none());
        }

        /// `parse_forwarded_ip` never panics on arbitrary input.
        #[test]
        fn parse_forwarded_ip_never_panics(s in ".*") {
            let _ = ProxyResolver::parse_forwarded_ip(&s);
        }

        /// Port-suffix normalisation: `ip:port` yields the same address as the
        /// bare `ip` (the port is stripped), for a valid IPv4 + port.
        #[test]
        fn parse_forwarded_ip_strips_port(
            a in any::<u8>(), b in any::<u8>(), c in any::<u8>(), d in any::<u8>(),
            port in any::<u16>(),
        ) {
            let ip = std::net::Ipv4Addr::new(a, b, c, d);
            let bare = ProxyResolver::parse_forwarded_ip(&ip.to_string());
            let with_port = ProxyResolver::parse_forwarded_ip(&format!("{ip}:{port}"));
            prop_assert_eq!(bare, Some(std::net::IpAddr::V4(ip)));
            prop_assert_eq!(with_port, Some(std::net::IpAddr::V4(ip)));
        }
    }
}
