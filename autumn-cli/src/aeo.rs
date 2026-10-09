//! `autumn aeo dns`: print the DNS-AID records to publish.
//!
//! An app cannot publish DNS records itself. This command writes the zone
//! file lines for the site's `_agents` namespace. It reads `[seo] base_url`
//! from `autumn.toml` (with the profile overlay) when `--base-url` is not
//! given. See `docs/guide/aeo.md`.

use autumn_web::aeo::dns_aid::{DnsAidInput, records, valid_mcp_path};

/// Run `autumn aeo dns` and print the zone lines.
pub fn dns(base_url: Option<String>, mcp_path: Option<&str>, ttl: u32, profile: Option<&str>) {
    let base_url = base_url.or_else(|| {
        let profile = crate::migrate::effective_profile(profile);
        configured_base_url(
            crate::migrate::read_autumn_toml_table_with_profile_from_config_dir(Some(&profile))
                .as_ref(),
        )
    });
    match render(base_url.as_deref(), mcp_path, ttl) {
        Ok(zone) => print!("{zone}"),
        Err(err) => {
            eprintln!("error: {err}");
            std::process::exit(1);
        }
    }
}

/// `[seo] base_url` from a parsed `autumn.toml`.
fn configured_base_url(table: Option<&toml::Table>) -> Option<String> {
    table?
        .get("seo")?
        .get("base_url")?
        .as_str()
        .map(str::to_owned)
}

/// The zone text for `base_url`, with a DNSSEC reminder.
fn render(base_url: Option<&str>, mcp_path: Option<&str>, ttl: u32) -> Result<String, String> {
    let base_url = base_url.ok_or_else(|| {
        "no base URL: pass --base-url or set [seo] base_url in autumn.toml".to_owned()
    })?;
    if let Some(path) = mcp_path.filter(|p| !valid_mcp_path(p)) {
        return Err(format!(
            "--mcp-path {path:?} is not an absolute path without a query, fragment, \
             quote or whitespace"
        ));
    }
    let lines = records(&DnsAidInput::new(base_url, mcp_path).ttl(ttl));
    if lines.is_empty() {
        return Err(format!(
            "{base_url:?} is not an http(s) URL with a DNS host name and no query or fragment"
        ));
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
    fn base_url_comes_from_the_seo_table() {
        let table: toml::Table =
            toml::from_str("[seo]\nbase_url = \"https://example.com\"\n").unwrap();
        assert_eq!(
            configured_base_url(Some(&table)).as_deref(),
            Some("https://example.com")
        );
        assert_eq!(configured_base_url(None), None);
    }

    #[test]
    fn needs_an_absolute_base_url() {
        assert!(render(None, None, 3600).unwrap_err().contains("--base-url"));
        assert!(render(Some("example.com"), None, 3600).is_err());
    }

    #[test]
    fn needs_an_absolute_mcp_path() {
        let err = render(Some("https://example.com"), Some("mcp"), 3600).unwrap_err();
        assert!(err.contains("--mcp-path"), "{err}");
        assert!(render(Some("https://example.com"), Some("/mcp?tenant=a"), 3600).is_err());
    }
}
