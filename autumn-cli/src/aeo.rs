//! `autumn aeo dns`: print the DNS-AID records to publish.
//!
//! An app cannot publish DNS records itself. This command writes the zone
//! file lines for the site's `_agents` namespace. It reads `[seo] base_url`
//! from `autumn.toml` when `--base-url` is not given. See
//! `docs/guide/aeo.md`.

use autumn_web::aeo::dns_aid::{DnsAidInput, records};

/// Run `autumn aeo dns` and print the zone lines.
pub fn dns(base_url: Option<String>, mcp_path: Option<String>, ttl: u32) {
    let base_url = base_url.or_else(|| {
        crate::migrate::read_autumn_toml_table()
            .as_ref()
            .and_then(|t| t.get("seo"))
            .and_then(|s| s.get("base_url"))
            .and_then(toml::Value::as_str)
            .map(str::to_owned)
    });
    match render(base_url.as_deref(), mcp_path.as_deref(), ttl) {
        Ok(zone) => print!("{zone}"),
        Err(err) => {
            eprintln!("error: {err}");
            std::process::exit(1);
        }
    }
}

/// The zone text for `base_url`, with a DNSSEC reminder.
fn render(base_url: Option<&str>, mcp_path: Option<&str>, ttl: u32) -> Result<String, String> {
    let base_url = base_url
        .ok_or("no base URL: pass --base-url or set [seo] base_url in autumn.toml".to_owned())?;
    let mut input = DnsAidInput::new(base_url, mcp_path);
    input.ttl = Some(ttl);
    let lines = records(&input);
    if lines.is_empty() {
        return Err(format!("{base_url:?} is not an absolute URL with a host"));
    }
    let mut zone = String::from(
        "; DNS-AID records (draft-mozleywilliams-dnsop-dnsaid).\n\
         ; Sign the zone with DNSSEC so validating resolvers trust them.\n",
    );
    for line in lines {
        zone.push_str(&line);
        zone.push('\n');
    }
    Ok(zone)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_records_with_a_dnssec_note() {
        let zone = render(Some("https://example.com"), Some("/mcp"), 600).unwrap();
        assert!(zone.starts_with("; DNS-AID records"), "{zone}");
        assert!(
            zone.contains("_mcp._agents.example.com. 600 IN SVCB 1 example.com."),
            "{zone}"
        );
        assert!(
            zone.contains("_catalog._agents.example.com. 600 IN TXT"),
            "{zone}"
        );
    }

    #[test]
    fn needs_an_absolute_base_url() {
        assert!(render(None, None, 3600).unwrap_err().contains("--base-url"));
        assert!(render(Some("example.com"), None, 3600).is_err());
    }
}
