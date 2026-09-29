use axum::extract::ConnectInfo;
use axum::http::Request;
use std::net::{IpAddr, SocketAddr};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrustedProxy {
    pub network: IpAddr,
    pub prefix_len: u8,
}

impl TrustedProxy {
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
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

    #[must_use]
    pub fn contains(&self, ip: IpAddr) -> bool {
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

/// Extract the originating client IP from a request, honoring the trusted-proxy policy.
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

fn is_trusted_proxy(ip: IpAddr, trusted_proxies: &[TrustedProxy]) -> bool {
    trusted_proxies
        .iter()
        .any(|trusted_proxy| trusted_proxy.contains(ip))
}

#[cfg(test)]
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
