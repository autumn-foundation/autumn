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
}

/// Zone file lines for the DNS-AID records. Empty when `base_url` has no
/// host.
#[must_use]
pub fn records(input: &DnsAidInput) -> Vec<String> {
    let base = input.base_url.trim().trim_end_matches('/');
    let Ok(url) = url::Url::parse(base) else {
        return Vec::new();
    };
    let Some(host) = url.host_str().map(str::to_ascii_lowercase) else {
        return Vec::new();
    };
    let port = url.port_or_known_default().unwrap_or(443);
    let ttl = input.ttl.unwrap_or(3600);
    let mut lines = Vec::new();
    if let Some(mcp) = &input.mcp_path {
        lines.push(format!(
            "_mcp._agents.{host}. {ttl} IN SVCB 1 {host}. alpn=\"mcp,h2\" port={port} \
             mandatory=alpn,port key65400=\"{base}{mcp}/server-card\""
        ));
    }
    lines.push(format!(
        "_index._agents.{host}. {ttl} IN SVCB 1 {host}. alpn=\"h2\" port={port}"
    ));
    if input.mcp_path.is_some() {
        lines.push(format!(
            "_index._agents.{host}. {ttl} IN TXT \"agents=mcp:mcp\""
        ));
    }
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
                "_index._agents.example.com. 3600 IN TXT \"agents=mcp:mcp\"",
                "_catalog._agents.example.com. 3600 IN TXT \
                 \"url=https://example.com/.well-known/ai-catalog.json\"",
            ]
        );
    }

    #[test]
    fn content_site_records_and_ports() {
        let mut input = DnsAidInput::new("http://App.Example.org:8080", None);
        input.ttl = Some(300);
        let lines = records(&input);
        assert_eq!(
            lines,
            vec![
                "_index._agents.app.example.org. 300 IN SVCB 1 app.example.org. alpn=\"h2\" port=8080",
                "_catalog._agents.app.example.org. 300 IN TXT \
                 \"url=http://App.Example.org:8080/.well-known/ai-catalog.json\"",
            ]
        );
        assert!(records(&DnsAidInput::new("not a url", None)).is_empty());
    }
}
