use axum::extract::ConnectInfo;
use axum::http::Request;
use std::net::{IpAddr, SocketAddr};

use crate::security::trusted_proxies::CidrRange;

/// A trusted-proxy CIDR range.
///
/// Nothing in the framework uses this type. Use
/// [`ProxyResolver`](crate::security::ProxyResolver) instead.
/// `ProxyResolver` uses the same matcher.
#[deprecated(
    since = "0.9.0",
    note = "use `ProxyResolver`; this type will be removed in the next breaking release"
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrustedProxy {
    pub network: IpAddr,
    pub prefix_len: u8,
}

#[allow(deprecated)]
impl TrustedProxy {
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        CidrRange::parse(value).map(|r| Self {
            network: r.network,
            prefix_len: r.prefix_len,
        })
    }

    #[must_use]
    pub fn contains(&self, ip: IpAddr) -> bool {
        CidrRange {
            network: self.network,
            prefix_len: self.prefix_len,
        }
        .contains(ip)
    }
}

/// Extract the originating client IP from a request, honoring the trusted-proxy policy.
///
/// Nothing in the framework uses this function. Use
/// [`ProxyResolver`](crate::security::ProxyResolver) instead.
#[deprecated(
    since = "0.9.0",
    note = "use `ProxyResolver`; this function will be removed in the next breaking release"
)]
#[allow(deprecated)]
pub fn extract_client_ip<B>(
    req: &Request<B>,
    trust_forwarded_headers: bool,
    trusted_proxies: &[TrustedProxy],
    trusted_proxies_configured: bool,
) -> Option<IpAddr> {
    let peer_ip = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| addr.ip())
        .or_else(|| req.extensions().get::<SocketAddr>().map(SocketAddr::ip));

    if trust_forwarded_headers {
        let allowed = if trusted_proxies_configured {
            peer_ip.is_some_and(|ip| is_trusted_proxy(ip, trusted_proxies))
        } else {
            true
        };

        if allowed {
            let all_xff: Vec<&str> = req
                .headers()
                .get_all("x-forwarded-for")
                .iter()
                .filter_map(|v| v.to_str().ok())
                .collect();

            if !all_xff.is_empty() {
                let joined = all_xff.join(", ");
                if let Some(xff_ip) = client_ip_from_x_forwarded_for(
                    &joined,
                    peer_ip,
                    trusted_proxies,
                    trusted_proxies_configured,
                ) {
                    return Some(xff_ip);
                }
            }

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
        }
    }

    peer_ip
}

#[allow(deprecated)]
fn client_ip_from_x_forwarded_for(
    header: &str,
    peer_ip: Option<IpAddr>,
    trusted_proxies: &[TrustedProxy],
    trusted_proxies_configured: bool,
) -> Option<IpAddr> {
    if !trusted_proxies_configured {
        let mut entries = header.rsplit(',').map(str::trim).filter(|s| !s.is_empty());
        let last = entries.next()?;

        if let Ok(last_ip) = last.parse::<IpAddr>() {
            if peer_ip.is_some_and(|peer_ip| last_ip == peer_ip)
                && let Some(prev) = entries.next()
                && let Ok(prev_ip) = prev.parse::<IpAddr>()
            {
                return Some(prev_ip);
            }
            return Some(last_ip);
        }
        return None;
    }

    for entry in header.rsplit(',').map(str::trim).filter(|s| !s.is_empty()) {
        if let Ok(ip) = entry.parse::<IpAddr>()
            && !is_trusted_proxy(ip, trusted_proxies)
        {
            return Some(ip);
        }
    }

    None
}

#[allow(deprecated)]
fn is_trusted_proxy(ip: IpAddr, trusted_proxies: &[TrustedProxy]) -> bool {
    trusted_proxies
        .iter()
        .any(|trusted_proxy| trusted_proxy.contains(ip))
}

#[cfg(test)]
#[allow(deprecated)]
mod tests {
    use super::*;

    #[test]
    fn parse_trusted_proxy_valid_ipv4_no_prefix() {
        let proxy = TrustedProxy::parse("192.168.1.1").unwrap();
        assert_eq!(proxy.network, "192.168.1.1".parse::<IpAddr>().unwrap());
        assert_eq!(proxy.prefix_len, 32);
    }

    #[test]
    fn parse_trusted_proxy_valid_ipv6_no_prefix() {
        let proxy = TrustedProxy::parse("2001:db8::1").unwrap();
        assert_eq!(proxy.network, "2001:db8::1".parse::<IpAddr>().unwrap());
        assert_eq!(proxy.prefix_len, 128);
    }

    #[test]
    fn parse_trusted_proxy_valid_ipv4_with_prefix() {
        let proxy = TrustedProxy::parse("10.0.0.0/8").unwrap();
        assert_eq!(proxy.network, "10.0.0.0".parse::<IpAddr>().unwrap());
        assert_eq!(proxy.prefix_len, 8);
    }

    #[test]
    fn parse_trusted_proxy_valid_ipv6_with_prefix() {
        let proxy = TrustedProxy::parse("2001:db8::/32").unwrap();
        assert_eq!(proxy.network, "2001:db8::".parse::<IpAddr>().unwrap());
        assert_eq!(proxy.prefix_len, 32);
    }

    #[test]
    fn parse_trusted_proxy_invalid_ip() {
        assert!(TrustedProxy::parse("not.an.ip").is_none());
        assert!(TrustedProxy::parse("256.256.256.256").is_none());
    }

    #[test]
    fn parse_trusted_proxy_invalid_prefix() {
        assert!(TrustedProxy::parse("192.168.1.1/33").is_none());
        assert!(TrustedProxy::parse("2001:db8::/129").is_none());
        assert!(TrustedProxy::parse("192.168.1.1/abc").is_none());
    }

    #[test]
    fn parse_trusted_proxy_empty_or_whitespace() {
        assert!(TrustedProxy::parse("").is_none());
        assert!(TrustedProxy::parse("   ").is_none());
    }

    #[test]
    fn contains_ipv4_exact_match() {
        let proxy = TrustedProxy::parse("192.168.1.1").unwrap();
        assert!(proxy.contains("192.168.1.1".parse().unwrap()));
        assert!(!proxy.contains("192.168.1.2".parse().unwrap()));
    }

    #[test]
    fn contains_ipv6_exact_match() {
        let proxy = TrustedProxy::parse("2001:db8::1").unwrap();
        assert!(proxy.contains("2001:db8::1".parse().unwrap()));
        assert!(!proxy.contains("2001:db8::2".parse().unwrap()));
    }

    #[test]
    fn contains_ipv4_cidr() {
        let proxy = TrustedProxy::parse("10.0.0.0/8").unwrap();
        assert!(proxy.contains("10.1.2.3".parse().unwrap()));
        assert!(proxy.contains("10.255.255.255".parse().unwrap()));
        assert!(!proxy.contains("11.0.0.0".parse().unwrap()));
    }

    #[test]
    fn contains_ipv6_cidr() {
        let proxy = TrustedProxy::parse("2001:db8::/32").unwrap();
        assert!(proxy.contains("2001:db8:1234::1".parse().unwrap()));
        assert!(!proxy.contains("2001:db9::1".parse().unwrap()));
    }

    #[test]
    fn contains_zero_prefix() {
        let proxy4 = TrustedProxy::parse("0.0.0.0/0").unwrap();
        assert!(proxy4.contains("1.2.3.4".parse().unwrap()));
        assert!(proxy4.contains("255.255.255.255".parse().unwrap()));
        assert!(!proxy4.contains("2001:db8::1".parse().unwrap())); // Mismatched family

        let proxy6 = TrustedProxy::parse("::/0").unwrap();
        assert!(proxy6.contains("2001:db8::1".parse().unwrap()));
        assert!(proxy6.contains("::1".parse().unwrap()));
        assert!(!proxy6.contains("1.2.3.4".parse().unwrap())); // Mismatched family
    }

    #[test]
    fn contains_mismatched_family() {
        let proxy4 = TrustedProxy::parse("192.168.1.0/24").unwrap();
        assert!(!proxy4.contains("2001:db8::1".parse().unwrap()));

        let proxy6 = TrustedProxy::parse("2001:db8::/32").unwrap();
        assert!(!proxy6.contains("192.168.1.1".parse().unwrap()));
    }

    // One CIDR rule: the deprecated type must agree with `CidrRange`.
    mod single_matcher {
        use super::*;
        use crate::security::trusted_proxies::CidrRange;
        use proptest::prelude::*;

        fn spec() -> impl Strategy<Value = String> {
            let ip = prop_oneof![
                any::<u32>().prop_map(|n| std::net::Ipv4Addr::from(n).to_string()),
                any::<u128>().prop_map(|n| std::net::Ipv6Addr::from(n).to_string()),
            ];
            (ip, proptest::option::of(0u16..=200), "[ \t]{0,2}").prop_map(|(ip, p, pad)| {
                p.map_or_else(
                    || format!("{pad}{ip}{pad}"),
                    |p| format!("{pad}{ip}/{p}{pad}"),
                )
            })
        }

        fn candidate() -> impl Strategy<Value = IpAddr> {
            prop_oneof![
                any::<u32>().prop_map(|n| IpAddr::from(std::net::Ipv4Addr::from(n))),
                any::<u128>().prop_map(|n| IpAddr::from(std::net::Ipv6Addr::from(n))),
            ]
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(512))]

            #[test]
            fn parse_agrees_with_cidr_range(s in prop_oneof![spec(), ".*"]) {
                let old = TrustedProxy::parse(&s).map(|p| (p.network, p.prefix_len));
                let new = CidrRange::parse(&s).map(|p| (p.network, p.prefix_len));
                prop_assert_eq!(old, new);
            }

            #[test]
            fn contains_agrees_with_cidr_range(s in spec(), ip in candidate()) {
                let old = TrustedProxy::parse(&s);
                let new = CidrRange::parse(&s);
                prop_assert_eq!(old.is_some(), new.is_some());
                if let (Some(old), Some(new)) = (old, new) {
                    prop_assert_eq!(old.contains(ip), new.contains(ip));
                }
            }
        }
    }

    fn make_req_with_peer(ip: &str) -> Request<()> {
        let mut req = Request::new(());
        let addr = format!("{ip}:12345").parse::<SocketAddr>().unwrap();
        req.extensions_mut().insert(ConnectInfo(addr));
        req
    }

    #[test]
    fn extract_ip_no_forwarded_headers_trust_disabled() {
        let req = make_req_with_peer("192.168.1.1");
        let ip = extract_client_ip(&req, false, &[], false);
        assert_eq!(ip, Some("192.168.1.1".parse().unwrap()));
    }

    #[test]
    fn extract_ip_no_forwarded_headers_trust_enabled() {
        let req = make_req_with_peer("192.168.1.1");
        let ip = extract_client_ip(&req, true, &[], false);
        assert_eq!(ip, Some("192.168.1.1".parse().unwrap()));
    }

    #[test]
    fn extract_ip_xff_untrusted_proxy() {
        let mut req = make_req_with_peer("192.168.1.1");
        req.headers_mut()
            .insert("x-forwarded-for", "1.2.3.4".parse().unwrap());

        // proxy 192.168.1.1 is NOT in the trusted list, so XFF is ignored
        let trusted = vec![TrustedProxy::parse("10.0.0.0/8").unwrap()];
        let ip = extract_client_ip(&req, true, &trusted, true);
        assert_eq!(ip, Some("192.168.1.1".parse().unwrap()));
    }

    #[test]
    fn extract_ip_xff_trusted_proxy() {
        let mut req = make_req_with_peer("10.0.0.1");
        req.headers_mut()
            .insert("x-forwarded-for", "1.2.3.4".parse().unwrap());

        // proxy 10.0.0.1 IS in the trusted list, so XFF is respected
        let trusted = vec![TrustedProxy::parse("10.0.0.0/8").unwrap()];
        let ip = extract_client_ip(&req, true, &trusted, true);
        assert_eq!(ip, Some("1.2.3.4".parse().unwrap()));
    }

    #[test]
    fn extract_ip_xff_chain_trust_configured() {
        let mut req = make_req_with_peer("10.0.0.1");
        req.headers_mut()
            .insert("x-forwarded-for", "1.2.3.4, 10.0.0.2".parse().unwrap());

        let trusted = vec![TrustedProxy::parse("10.0.0.0/8").unwrap()];
        // Peer (10.0.0.1) is trusted.
        // Last XFF (10.0.0.2) is trusted.
        // Second to last XFF (1.2.3.4) is NOT trusted. It's the client.
        let ip = extract_client_ip(&req, true, &trusted, true);
        assert_eq!(ip, Some("1.2.3.4".parse().unwrap()));
    }

    #[test]
    fn extract_ip_xff_chain_trust_not_configured() {
        let mut req = make_req_with_peer("10.0.0.1");
        req.headers_mut()
            .insert("x-forwarded-for", "1.2.3.4, 10.0.0.1".parse().unwrap());

        // If trusted proxies are not explicitly configured, we only trust the peer IP
        // to have appended its own address, so we take the second to last.
        let ip = extract_client_ip(&req, true, &[], false);
        assert_eq!(ip, Some("1.2.3.4".parse().unwrap()));
    }

    #[test]
    fn extract_ip_x_real_ip_trusted() {
        let mut req = make_req_with_peer("10.0.0.1");
        req.headers_mut()
            .insert("x-real-ip", "1.2.3.4".parse().unwrap());

        let trusted = vec![TrustedProxy::parse("10.0.0.0/8").unwrap()];
        let ip = extract_client_ip(&req, true, &trusted, true);
        assert_eq!(ip, Some("1.2.3.4".parse().unwrap()));
    }
}
