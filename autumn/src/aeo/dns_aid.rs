//! DNS for AI Discovery (DNS-AID) records.
//!
//! An app cannot publish DNS records itself. This module writes the zone
//! file lines to publish under the `_agents` namespace, following
//! `draft-mozleywilliams-dnsop-dnsaid` and the names that
//! isitagentready.com queries. Sign the zone with DNSSEC: validating
//! resolvers then mark the answers as authenticated.
//!
//! The draft's SVCB keys are not registered yet, so the lines use the
//! private-use numbers of the reference implementation (`key65400` = `cap`).

/// Input for [`records`].
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct DnsAidInput {
    /// Site base URL, e.g. `https://example.com`.
    pub base_url: String,
    /// MCP mount path, when the app serves MCP.
    pub mcp_path: Option<String>,
    /// Record TTL in seconds. Default: 3600.
    pub ttl: Option<u32>,
}

impl DnsAidInput {
    /// Input for `base_url` with an optional MCP path.
    #[must_use]
    pub fn new(base_url: impl Into<String>, mcp_path: Option<&str>) -> Self {
        Self {
            base_url: base_url.into(),
            mcp_path: mcp_path.map(str::to_owned),
            ttl: None,
        }
    }

    /// Set the record TTL in seconds.
    #[must_use]
    pub const fn ttl(mut self, secs: u32) -> Self {
        self.ttl = Some(secs);
        self
    }
}

/// Whether `path` can be written into a record as an MCP mount path.
///
/// It must start with `/` and hold only visible ASCII other than `?`, `#`,
/// `"` and `\`. A query or a fragment would swallow the appended
/// `/server-card`, and a quote or a backslash would break the zone file
/// string.
#[must_use]
pub fn valid_mcp_path(path: &str) -> bool {
    path.starts_with('/') && zone_safe(path) && !path.contains(['?', '#'])
}

/// Visible ASCII other than `"` and `\`: safe inside a quoted zone string.
fn zone_safe(s: &str) -> bool {
    s.bytes()
        .all(|b| b.is_ascii_graphic() && b != b'"' && b != b'\\')
}

/// Zone file lines for the DNS-AID records.
///
/// Empty when `base_url` is not an
/// `http` or `https` URL with a DNS host name (an IP address has no zone),
/// has a query or a fragment, or holds a character a zone file string
/// cannot carry; or when `mcp_path` fails [`valid_mcp_path`].
#[must_use]
pub fn records(input: &DnsAidInput) -> Vec<String> {
    let base = input.base_url.trim().trim_end_matches('/');
    if !zone_safe(base)
        || input
            .mcp_path
            .as_deref()
            .is_some_and(|p| !valid_mcp_path(p))
    {
        return Vec::new();
    }
    let Ok(url) = url::Url::parse(base) else {
        return Vec::new();
    };
    // The records describe an HTTP service: another scheme's port and URL
    // would be wrong. Paths are appended to the base, so a query or a
    // fragment would swallow them.
    if !matches!(url.scheme(), "http" | "https")
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Vec::new();
    }
    let Some(url::Host::Domain(host)) = url.host() else {
        return Vec::new();
    };
    let host = host.to_ascii_lowercase();
    let alpn = if url.scheme() == "https" {
        "h2"
    } else {
        "http/1.1"
    };
    let port = url.port_or_known_default().unwrap_or(443);
    let ttl = input.ttl.unwrap_or(3600);
    let mut lines = Vec::new();
    if let Some(mcp) = &input.mcp_path {
        lines.push(format!(
            "_mcp._agents.{host}. {ttl} IN SVCB 1 {host}. alpn=\"mcp,{alpn}\" port={port} \
             mandatory=alpn,port key65400=\"{base}{card}\"",
            card = super::documents::server_card_path(mcp)
        ));
    }
    lines.push(format!(
        "_index._agents.{host}. {ttl} IN SVCB 1 {host}. alpn=\"{alpn}\" port={port}"
    ));
    lines.push(format!(
        "_catalog._agents.{host}. {ttl} IN TXT \"url={base}{}\"",
        super::AI_CATALOG_PATH
    ));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_site_records() {
        let lines = records(&DnsAidInput::new("https://example.com/", Some("/mcp")));
        assert_eq!(
            lines,
            vec![
                "_mcp._agents.example.com. 3600 IN SVCB 1 example.com. alpn=\"mcp,h2\" port=443 \
                 mandatory=alpn,port key65400=\"https://example.com/mcp/server-card\"",
                "_index._agents.example.com. 3600 IN SVCB 1 example.com. alpn=\"h2\" port=443",
                "_catalog._agents.example.com. 3600 IN TXT \
                 \"url=https://example.com/.well-known/ai-catalog.json\"",
            ]
        );
    }

    #[test]
    fn content_site_records_and_ports() {
        let lines = records(&DnsAidInput::new("http://App.Example.org:8080", None).ttl(300));
        assert_eq!(
            lines,
            vec![
                "_index._agents.app.example.org. 300 IN SVCB 1 app.example.org. \
                 alpn=\"http/1.1\" port=8080",
                "_catalog._agents.app.example.org. 300 IN TXT \
                 \"url=http://App.Example.org:8080/.well-known/ai-catalog.json\"",
            ]
        );
        assert!(records(&DnsAidInput::new("not a url", None)).is_empty());
        assert!(records(&DnsAidInput::new("https://127.0.0.1", None)).is_empty());
        assert!(records(&DnsAidInput::new("ftp://example.com", None)).is_empty());
        assert!(records(&DnsAidInput::new("https://example.com?tenant=a", None)).is_empty());
        assert!(records(&DnsAidInput::new("https://example.com#top", None)).is_empty());
        assert!(records(&DnsAidInput::new("https://exa\nmple.com", None)).is_empty());
        assert!(records(&DnsAidInput::new("https://example.com/a\"b", None)).is_empty());
    }

    #[test]
    fn mcp_path_must_be_a_plain_absolute_path() {
        for bad in [
            "mcp",
            "/mcp?tenant=a",
            "/mcp#x",
            "/m\"cp",
            "/m cp",
            "/mcp\n",
            "",
        ] {
            assert!(!valid_mcp_path(bad), "{bad:?}");
            assert!(
                records(&DnsAidInput::new("https://example.com", Some(bad))).is_empty(),
                "{bad:?}"
            );
        }
        assert!(valid_mcp_path("/mcp"));
        assert!(valid_mcp_path("/api/mcp/"));
    }
}
