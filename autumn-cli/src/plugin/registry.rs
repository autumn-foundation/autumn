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
            let version = pick_version(entry).unwrap_or_default();
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

/// The version to install for a crates.io crate object.
///
/// `max_stable_version` first: these three fields are not spellings of one
/// value. `newest_version` is the most *recently published* version, so a
/// back-ported `1.4.3` released after `2.0.0` — or a trailing pre-release —
/// would be picked over the newest release. `max_version` is the highest
/// version including pre-releases. Only `max_stable_version` is what a user
/// asking for "the current version" means; the other two are fallbacks for
/// mirrors that omit it.
fn pick_version(entry: &serde_json::Value) -> Option<String> {
    ["max_stable_version", "max_version", "newest_version"]
        .into_iter()
        .find_map(|field| entry.get(field).and_then(serde_json::Value::as_str))
        .map(std::borrow::ToOwned::to_owned)
}

/// Collapse a crates.io description to a single line for table rendering.
///
/// Control characters are replaced, not just collapsed: a description is
/// attacker-supplied text (anyone can publish `autumn-plugin-<x>`) that
/// `plugin list` prints straight to a terminal, and `char::is_whitespace` is
/// false for `ESC`. Without this an `\x1b[…` sequence in a description could
/// clear the victim's screen, spoof output, retitle the window, or reach the
/// clipboard through OSC 52 — from a bare `autumn plugin list`, with nothing
/// installed.
fn one_line(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
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
    // The SAME field preference as the search path. Diverging here let
    // `plugin add` pin a back-ported or pre-release version while
    // `plugin list` had shown the stable one the user picked.
    pick_version(value.get("crate")?)
}

/// Largest response body this module will read into memory.
///
/// `SEARCH_TIMEOUT` bounds time, not bytes, and `per_page=50` bounds the
/// result count, not the payload — so without a cap a hostile or broken
/// endpoint could OOM the CLI. 4 MiB is two orders of magnitude above a real
/// 50-result crates.io page.
const MAX_RESPONSE_BYTES: u64 = 4 * 1024 * 1024;

/// One blocking GET, or `None` for any failure at all.
///
/// Redirects are capped (there is no crates.io API endpoint that needs them,
/// and an uncapped chain is what would let the size cap be applied to a body
/// from some other host), and the body is read through a byte limit.
fn get(url: &str) -> Option<String> {
    use std::io::Read as _;

    let client = reqwest::blocking::Client::builder()
        .timeout(SEARCH_TIMEOUT)
        .redirect(reqwest::redirect::Policy::limited(2))
        .user_agent(user_agent())
        .build()
        .ok()?;
    let response = client.get(url).send().ok()?;
    if !response.status().is_success() {
        return None;
    }
    let mut body = Vec::new();
    response
        .take(MAX_RESPONSE_BYTES)
        .read_to_end(&mut body)
        .ok()?;
    String::from_utf8(body).ok()
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

    /// `newest_version` is a fallback for mirrors that omit the others.
    #[test]
    fn either_version_field_is_accepted() {
        let found = parse_search_response(SAMPLE);
        let audit = found
            .iter()
            .find(|c| c.crate_name == "autumn-plugin-audit")
            .expect("audit");
        assert_eq!(audit.version, "1.0.0");
    }

    /// The three version fields are not spellings of one value:
    /// `newest_version` is the most recently PUBLISHED version, so a
    /// back-ported release or a trailing pre-release would win over the
    /// current one. `max_stable_version` is what a user means.
    #[test]
    fn the_max_stable_version_wins_over_the_newest_publish() {
        let body = r#"{"crates":[{"name":"autumn-plugin-x","max_stable_version":"2.0.0",
            "max_version":"2.1.0-rc.1","newest_version":"1.4.3","description":"back-ported"}]}"#;
        let found = parse_search_response(body);
        assert_eq!(found[0].version, "2.0.0");
    }

    /// A crates.io description is attacker-supplied text printed straight to a
    /// terminal. `ESC` is not Unicode whitespace, so collapsing whitespace
    /// alone leaves an ANSI sequence intact.
    #[test]
    fn control_characters_never_survive_into_the_summary() {
        let body = "{\"crates\":[{\"name\":\"autumn-plugin-x\",\"max_version\":\"0.1.0\",\
                    \"description\":\"safe\\u001b[2J\\u0007evil\"}]}";
        let found = parse_search_response(body);
        assert_eq!(found.len(), 1);
        assert!(
            !found[0].summary.chars().any(char::is_control),
            "{:?}",
            found[0].summary
        );
        // The ESC and BEL are gone; what remains is inert printable text, which
        // is the point — the sequence can no longer drive the terminal.
        assert_eq!(found[0].summary, "safe [2J evil");
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
