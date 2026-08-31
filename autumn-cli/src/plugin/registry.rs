//! Community-plugin discovery over crates.io.
//!
//! Issue #1606 scopes discovery to "crates.io plus the existing naming
//! convention" — no hosted registry. So this is one search call against the
//! public crates.io API for the documented `autumn-plugin-` prefix, filtered
//! to names that actually carry it (the API's relevance search happily returns
//! neighbours).
//!
//! Network failure is **not** an error: `autumn plugin list` still has the
//! whole first-party catalog to show, so a failed or skipped lookup becomes a
//! note in the output rather than a non-zero exit.

use std::time::Duration;

/// A community plugin crate found on crates.io.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommunityPlugin {
    /// crates.io name, e.g. `autumn-plugin-live-feed`.
    pub crate_name: String,
    /// Newest published version.
    pub version: String,
    /// crates.io description, trimmed to one line.
    pub summary: String,
}

/// How long to wait on crates.io before giving up and rendering the
/// first-party catalog alone.
pub const SEARCH_TIMEOUT: Duration = Duration::from_secs(5);

/// The crates.io search endpoint for the documented naming convention.
#[must_use]
pub fn search_url() -> String {
    format!(
        "https://crates.io/api/v1/crates?q={}&per_page=50",
        super::catalog::COMMUNITY_PREFIX
    )
}

/// The `User-Agent` crates.io asks API clients to identify themselves with.
fn user_agent() -> String {
    format!(
        "autumn-cli/{} (https://github.com/autumn-foundation/autumn)",
        env!("CARGO_PKG_VERSION")
    )
}

/// Parse a crates.io search response body into community plugins.
///
/// Split from the HTTP call so the filtering rules are unit-testable without
/// a network. crates.io ranks by relevance, so the response also carries
/// neighbours (`autumn-web` itself, unrelated crates): only names that
/// actually carry the documented prefix are plugins. Results come back sorted
/// by name so the listing is stable across runs.
#[must_use]
pub fn parse_search_response(body: &str) -> Vec<CommunityPlugin> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return Vec::new();
    };
    let Some(crates) = value.get("crates").and_then(serde_json::Value::as_array) else {
        return Vec::new();
    };
    let mut found: Vec<CommunityPlugin> = crates
        .iter()
        .filter_map(|entry| {
            let name = entry.get("name")?.as_str()?;
            if !super::catalog::is_community_name(name) {
                return None;
            }
            // `newest_version` is today's field; `max_version` is the older
            // spelling still returned by some mirrors.
            let version = entry
                .get("newest_version")
                .or_else(|| entry.get("max_version"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned();
            Some(CommunityPlugin {
                crate_name: name.to_owned(),
                version,
                summary: one_line(
                    entry
                        .get("description")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default(),
                ),
            })
        })
        .collect();
    found.sort_by(|a, b| a.crate_name.cmp(&b.crate_name));
    found
}

/// Collapse a crates.io description to a single line for table rendering.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Query crates.io for community plugins. `None` when the lookup could not be
/// completed (offline, timeout, non-200, unparseable body).
///
/// Deliberately `None` rather than `Err`: `autumn plugin list` still has the
/// whole first-party catalog to render, so an unreachable crates.io is a note
/// in the output, not a failed command.
#[must_use]
pub fn search() -> Option<Vec<CommunityPlugin>> {
    let body = get(&search_url())?;
    Some(parse_search_response(&body))
}

/// Look up one crate's newest version. `None` when the lookup fails or the
/// crate does not exist.
#[must_use]
pub fn latest_version(crate_name: &str) -> Option<String> {
    let body = get(&format!("https://crates.io/api/v1/crates/{crate_name}"))?;
    let value = serde_json::from_str::<serde_json::Value>(&body).ok()?;
    let krate = value.get("crate")?;
    krate
        .get("newest_version")
        .or_else(|| krate.get("max_version"))
        .and_then(serde_json::Value::as_str)
        .map(std::borrow::ToOwned::to_owned)
}

/// One blocking GET, or `None` for any failure at all.
fn get(url: &str) -> Option<String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(SEARCH_TIMEOUT)
        .user_agent(user_agent())
        .build()
        .ok()?;
    let response = client.get(url).send().ok()?;
    if !response.status().is_success() {
        return None;
    }
    response.text().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
      "crates": [
        {"name": "autumn-plugin-live-feed", "max_version": "0.3.1", "description": "Live feeds for autumn-web"},
        {"name": "autumn-plugin-audit", "newest_version": "1.0.0", "description": "Audit trail"},
        {"name": "autumn-web", "max_version": "0.7.0", "description": "The framework itself"},
        {"name": "some-other-crate", "max_version": "2.0.0", "description": "unrelated"}
      ]
    }"#;

    /// crates.io relevance search returns neighbours; only names that actually
    /// carry the documented prefix are community plugins.
    #[test]
    fn only_convention_named_crates_are_kept() {
        let found = parse_search_response(SAMPLE);
        let names: Vec<&str> = found.iter().map(|c| c.crate_name.as_str()).collect();
        assert_eq!(names, ["autumn-plugin-audit", "autumn-plugin-live-feed"]);
    }

    #[test]
    fn version_and_summary_come_from_the_response() {
        let found = parse_search_response(SAMPLE);
        let feed = found
            .iter()
            .find(|c| c.crate_name == "autumn-plugin-live-feed")
            .expect("live-feed");
        assert_eq!(feed.version, "0.3.1");
        assert_eq!(feed.summary, "Live feeds for autumn-web");
    }

    /// `newest_version` is the field crates.io actually returns today;
    /// `max_version` is the older spelling. Both must work.
    #[test]
    fn either_version_field_is_accepted() {
        let found = parse_search_response(SAMPLE);
        let audit = found
            .iter()
            .find(|c| c.crate_name == "autumn-plugin-audit")
            .expect("audit");
        assert_eq!(audit.version, "1.0.0");
    }

    #[test]
    fn a_missing_description_becomes_an_empty_summary() {
        let body = r#"{"crates":[{"name":"autumn-plugin-x","max_version":"0.1.0"}]}"#;
        let found = parse_search_response(body);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].summary, "");
    }

    #[test]
    fn a_multi_line_description_is_flattened_to_one_line() {
        let body = r#"{"crates":[{"name":"autumn-plugin-x","max_version":"0.1.0","description":"first\nsecond"}]}"#;
        let found = parse_search_response(body);
        assert_eq!(found[0].summary, "first second");
    }

    #[test]
    fn a_garbage_body_yields_no_plugins() {
        assert!(parse_search_response("not json").is_empty());
        assert!(parse_search_response("{}").is_empty());
    }

    #[test]
    fn the_search_url_asks_crates_io_for_the_documented_prefix() {
        let url = search_url();
        assert!(url.starts_with("https://crates.io/api/v1/crates"), "{url}");
        assert!(url.contains("autumn-plugin-"), "{url}");
    }
}
