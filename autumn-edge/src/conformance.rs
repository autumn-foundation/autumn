//! The shared definition of "the edge reproduced the origin".
//!
//! AC-2 asks for byte-identical bodies and headers. Taken literally that is
//! unachievable and, worse, untestable: the origin stamps a `Date`, a request
//! id, and server-timing spans onto every response, none of which the edge has
//! any business inventing. So the guarantee is stated precisely instead, in
//! one place that both the documentation and the CI test read from:
//!
//! > **Status and body bytes are compared exactly. Headers are compared after
//! > projection**: [`VOLATILE_HEADERS`] and the internal fallthrough sentinel
//! > are dropped, names are lowercased, and the rest is canonically sorted.
//!
//! Everything the projection removes is a header the edge lane never emits by
//! design — the edge serves only what a handler produced. Anything else
//! differing is a real divergence and fails the test.
//!
//! Note what this does *not* weaken: a header a handler set is compared
//! exactly, present-for-present and value-for-value. The projection only
//! excuses the headers the origin's middleware stack adds.

use crate::route::EdgeCapability;
use crate::wire::{
    EdgeRequest, EdgeResponse, FALLTHROUGH_SENTINEL, FallthroughReason, canonicalize_headers,
};

/// Headers excluded from the byte-identity comparison.
///
/// Framework-owned and deliberately small: each entry is a header the origin's
/// middleware stack produces and the edge lane structurally cannot. Growing
/// this list weakens the guarantee, so it is one constant rather than a
/// per-test allowance.
///
/// `set-cookie` qualifies because the exclusion cannot mask a handler-set
/// cookie: the wire runtime *declines* any edge response carrying one (cookies
/// are origin-only session state), so on the edge side the header never
/// exists — only the origin's session middleware produces it.
pub const VOLATILE_HEADERS: &[&str] = &[
    "content-security-policy",
    "date",
    "server-timing",
    "set-cookie",
    "x-request-id",
];

/// Headers the origin's security middleware sets and a capsule does not.
///
/// The host sets them on an edge response (see
/// `EdgeGateway::with_response_headers`). [`compare_capsule`] excuses them,
/// because a capsule never sets them. [`compare`] does not: what the client
/// gets from the host must match the origin.
pub const SECURITY_HEADERS: &[&str] = &[
    "permissions-policy",
    "referrer-policy",
    "strict-transport-security",
    "x-content-type-options",
    "x-frame-options",
    "x-xss-protection",
];

/// CORS headers a CORS layer can send on every response.
///
/// The edge node copies them from the origin when they do not change (see
/// `node::origin_static_headers`). A policy that answers each request
/// `Origin` differently cannot be copied.
pub const CORS_HEADERS: &[&str] = &[
    "access-control-allow-credentials",
    "access-control-allow-origin",
    "access-control-expose-headers",
];

/// What a conformance case expects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Expectation {
    /// The edge is expected to answer.
    Served,
    /// The edge is expected to decline, for this reason.
    Fallthrough(FallthroughReason),
}

/// One request in the shared conformance corpus.
#[derive(Clone, Copy, Debug)]
pub struct ConformanceCase {
    /// Stable identifier used in assertion messages.
    pub name: &'static str,
    /// HTTP method.
    pub method: &'static str,
    /// Origin-form request target.
    pub uri: &'static str,
    /// Request headers.
    pub headers: &'static [(&'static str, &'static str)],
    /// Capabilities the host offers for this case.
    pub provided_capabilities: &'static [EdgeCapability],
    /// The expected outcome.
    pub expect: Expectation,
}

impl ConformanceCase {
    /// The wire request this case describes.
    #[must_use]
    pub fn request(&self) -> EdgeRequest {
        EdgeRequest {
            method: self.method.to_owned(),
            uri: self.uri.to_owned(),
            headers: self
                .headers
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect(),
            body: Vec::new(),
            identity: None,
        }
    }
}

/// Project headers into the comparable subset: drop [`VOLATILE_HEADERS`] and
/// the fallthrough sentinel, lowercase the names, sort canonically.
///
/// Idempotent, and applied to *both* sides of every comparison.
#[must_use]
pub fn project_headers(headers: &[(String, String)]) -> Vec<(String, String)> {
    let kept: Vec<(String, String)> = headers
        .iter()
        .filter(|(name, _)| {
            let name = name.to_ascii_lowercase();
            !VOLATILE_HEADERS.contains(&name.as_str()) && name != FALLTHROUGH_SENTINEL
        })
        .cloned()
        .collect();
    canonicalize_headers(&kept)
}

/// Count how many times each `(name, value)` pair occurs.
///
/// Plain equality (`Vec::contains`) only checks presence, not count. A pair
/// that appears twice on one side and once on the other must show up as a
/// difference, so `compare` counts occurrences instead.
fn count_pairs(
    headers: &[(String, String)],
) -> std::collections::BTreeMap<(String, String), usize> {
    let mut counts: std::collections::BTreeMap<(String, String), usize> =
        std::collections::BTreeMap::new();
    for pair in headers {
        let count = counts.entry(pair.clone()).or_insert(0_usize);
        *count = count.saturating_add(1);
    }
    counts
}

/// The result of comparing two responses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The edge reproduced the origin exactly.
    Reproduced,
    /// The edge diverged.
    Diverged {
        /// What differed.
        detail: String,
    },
}

/// Compare an origin response with an edge response.
///
/// Strict: the status must match exactly, the projected headers must match
/// exactly, and the body bytes must match exactly. The `detail` on a
/// divergence names what differed — the point of a conformance failure is that
/// a developer can act on it without re-running anything.
#[must_use]
pub fn compare(native: &EdgeResponse, edge: &EdgeResponse) -> Verdict {
    if native.status != edge.status {
        return Verdict::Diverged {
            detail: format!(
                "status: origin answered {} and the edge answered {}",
                native.status, edge.status
            ),
        };
    }

    let native_headers = project_headers(&native.headers);
    let edge_headers = project_headers(&edge.headers);
    if native_headers != edge_headers {
        let native_counts = count_pairs(&native_headers);
        let edge_counts = count_pairs(&edge_headers);
        let mut differences = Vec::new();
        for (pair, native_count) in &native_counts {
            let edge_count = edge_counts.get(pair).copied().unwrap_or(0);
            if edge_count == 0 {
                differences.push(format!(
                    "origin has `{}: {}` and the edge does not",
                    pair.0, pair.1
                ));
            } else if edge_count != *native_count {
                differences.push(format!(
                    "origin has `{}: {}` {} time(s) and the edge has it {} time(s)",
                    pair.0, pair.1, native_count, edge_count
                ));
            }
        }
        for pair in edge_counts.keys() {
            if !native_counts.contains_key(pair) {
                differences.push(format!(
                    "the edge has `{}: {}` and the origin does not",
                    pair.0, pair.1
                ));
            }
        }
        return Verdict::Diverged {
            detail: format!("headers: {}", differences.join("; ")),
        };
    }

    if native.body != edge.body {
        let first_difference = native
            .body
            .iter()
            .zip(edge.body.iter())
            .position(|(left, right)| left != right)
            .map_or_else(
                || "one body is a prefix of the other".to_owned(),
                |index| format!("first differing byte at offset {index}"),
            );
        return Verdict::Diverged {
            detail: format!(
                "body: origin produced {} bytes and the edge produced {} ({first_difference})",
                native.body.len(),
                edge.body.len()
            ),
        };
    }

    Verdict::Reproduced
}

/// Compare an origin response with a raw capsule response.
///
/// The same as [`compare`], but [`SECURITY_HEADERS`] and [`CORS_HEADERS`]
/// are dropped from both sides first: the host sets them, not the capsule.
/// Every header that is not volatile must match in both directions.
#[must_use]
pub fn compare_capsule(origin: &EdgeResponse, capsule: &EdgeResponse) -> Verdict {
    let host_set = |name: &str| SECURITY_HEADERS.contains(&name) || CORS_HEADERS.contains(&name);
    let without_security = |response: &EdgeResponse| EdgeResponse {
        status: response.status,
        headers: response
            .headers
            .iter()
            .filter(|(name, _)| !host_set(name.to_ascii_lowercase().as_str()))
            .cloned()
            .collect(),
        body: response.body.clone(),
    };
    compare(&without_security(origin), &without_security(capsule))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::FALLTHROUGH_SENTINEL;

    fn owned(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect()
    }

    fn response(headers: &[(&str, &str)], body: &str) -> EdgeResponse {
        EdgeResponse {
            status: 200,
            headers: owned(headers),
            body: body.as_bytes().to_vec(),
        }
    }

    #[test]
    fn projection_drops_volatile_headers_and_the_sentinel() {
        let projected = project_headers(&owned(&[
            ("Content-Type", "text/html"),
            ("Date", "Tue, 19 Aug 2026 00:00:00 GMT"),
            ("X-Request-Id", "abc"),
            ("server-timing", "app;dur=3"),
            ("Content-Security-Policy", "default-src 'self'"),
            ("set-cookie", "sid=1"),
            (FALLTHROUGH_SENTINEL, "unknown_route"),
            ("x-edge-lane", "edge"),
        ]));

        assert_eq!(
            projected,
            owned(&[("content-type", "text/html"), ("x-edge-lane", "edge")])
        );
    }

    #[test]
    fn projection_canonicalizes_what_it_keeps() {
        let projected = project_headers(&owned(&[
            ("X-Zed", "1"),
            ("Accept-Ranges", "bytes"),
            ("x-zed", "2"),
        ]));

        assert_eq!(
            projected,
            owned(&[("accept-ranges", "bytes"), ("x-zed", "1"), ("x-zed", "2")])
        );
    }

    #[test]
    fn projection_is_idempotent() {
        let once = project_headers(&owned(&[("B", "2"), ("Date", "x"), ("a", "1")]));
        assert_eq!(project_headers(&once), once);
    }

    #[test]
    fn identical_responses_reproduce() {
        let native = response(&[("content-type", "text/html")], "<h1>hi</h1>");
        let edge = response(&[("Content-Type", "text/html")], "<h1>hi</h1>");
        assert_eq!(compare(&native, &edge), Verdict::Reproduced);
    }

    #[test]
    fn a_volatile_header_alone_is_not_a_divergence() {
        let native = response(
            &[("content-type", "text/html"), ("date", "then")],
            "<h1>hi</h1>",
        );
        let edge = response(&[("content-type", "text/html")], "<h1>hi</h1>");
        assert_eq!(compare(&native, &edge), Verdict::Reproduced);
    }

    #[test]
    fn a_status_difference_diverges() {
        let native = response(&[], "hi");
        let edge = EdgeResponse {
            status: 404,
            ..response(&[], "hi")
        };
        let Verdict::Diverged { detail } = compare(&native, &edge) else {
            panic!("expected divergence");
        };
        assert!(detail.contains("200"), "{detail}");
        assert!(detail.contains("404"), "{detail}");
    }

    #[test]
    fn a_pure_multiplicity_difference_diverges_with_a_non_empty_detail() {
        // Same `(name, value)` pair, different counts: `Vec::contains` alone
        // cannot tell these apart, so the divergence detail must not come out
        // empty here.
        let native = response(&[("x-zed", "1"), ("x-zed", "1")], "hi");
        let edge = response(&[("x-zed", "1")], "hi");
        let Verdict::Diverged { detail } = compare(&native, &edge) else {
            panic!("expected divergence");
        };
        assert_ne!(
            detail, "headers: ",
            "the detail must not be empty: {detail}"
        );
        assert!(detail.contains("x-zed"), "{detail}");
        assert!(detail.contains('2'), "{detail}");
        assert!(detail.contains('1'), "{detail}");
    }

    #[test]
    fn a_header_difference_diverges() {
        let native = response(&[("x-edge-lane", "origin")], "hi");
        let edge = response(&[("x-edge-lane", "edge")], "hi");
        let Verdict::Diverged { detail } = compare(&native, &edge) else {
            panic!("expected divergence");
        };
        assert!(detail.contains("x-edge-lane"), "{detail}");
    }

    #[test]
    fn a_body_difference_diverges_and_reports_bytes() {
        let native = response(&[], "hi");
        let edge = response(&[], "hi ");
        let Verdict::Diverged { detail } = compare(&native, &edge) else {
            panic!("expected divergence");
        };
        assert!(detail.contains("body"), "{detail}");
    }

    #[test]
    fn a_case_renders_the_request_it_describes() {
        let case = ConformanceCase {
            name: "kv hit",
            method: "GET",
            uri: "/note/greeting",
            headers: &[("accept", "text/html")],
            provided_capabilities: &[EdgeCapability::Kv],
            expect: Expectation::Served,
        };

        let request = case.request();
        assert_eq!(request.method, "GET");
        assert_eq!(request.uri, "/note/greeting");
        assert_eq!(request.headers, owned(&[("accept", "text/html")]));
        assert!(request.body.is_empty());
    }

    #[test]
    fn the_capsule_comparison_excuses_only_security_headers() {
        let origin = response(
            &[
                ("content-type", "text/plain"),
                ("x-frame-options", "DENY"),
                ("x-content-type-options", "nosniff"),
                ("referrer-policy", "strict-origin-when-cross-origin"),
                ("date", "then"),
            ],
            "hi",
        );
        let capsule = response(&[("content-type", "text/plain")], "hi");
        assert_eq!(compare_capsule(&origin, &capsule), Verdict::Reproduced);
        assert!(matches!(
            compare(&origin, &capsule),
            Verdict::Diverged { .. }
        ));

        let origin_only = response(&[("content-type", "text/plain"), ("x-other", "1")], "hi");
        let Verdict::Diverged { detail } = compare_capsule(&origin_only, &capsule) else {
            panic!("a header only the origin sends must diverge");
        };
        assert!(detail.contains("x-other"), "{detail}");
    }

    #[test]
    fn the_capsule_comparison_excuses_the_cors_headers_the_host_sets() {
        let origin = response(
            &[
                ("content-type", "text/plain"),
                ("access-control-allow-origin", "*"),
            ],
            "hi",
        );
        let capsule = response(&[("content-type", "text/plain")], "hi");
        assert_eq!(compare_capsule(&origin, &capsule), Verdict::Reproduced);
    }

    #[test]
    fn the_cors_set_is_lowercase_sorted_and_disjoint_from_the_others() {
        let mut sorted = CORS_HEADERS.to_vec();
        sorted.sort_unstable();
        assert_eq!(CORS_HEADERS, sorted);
        for name in CORS_HEADERS {
            assert_eq!(*name, name.to_ascii_lowercase());
            assert!(!VOLATILE_HEADERS.contains(name), "{name}");
            assert!(!SECURITY_HEADERS.contains(name), "{name}");
        }
    }

    #[test]
    fn the_security_set_is_lowercase_sorted_and_disjoint_from_the_volatile_set() {
        let mut sorted = SECURITY_HEADERS.to_vec();
        sorted.sort_unstable();
        assert_eq!(SECURITY_HEADERS, sorted);
        for name in SECURITY_HEADERS {
            assert_eq!(*name, name.to_ascii_lowercase());
            assert!(!VOLATILE_HEADERS.contains(name), "{name}");
        }
    }

    #[test]
    fn the_volatile_set_is_lowercase_and_sorted_for_documentation_parity() {
        let mut sorted = VOLATILE_HEADERS.to_vec();
        sorted.sort_unstable();
        assert!(
            VOLATILE_HEADERS
                .iter()
                .all(|name| *name == name.to_ascii_lowercase()),
            "{VOLATILE_HEADERS:?}"
        );
        assert!(VOLATILE_HEADERS.contains(&"date"));
        assert!(VOLATILE_HEADERS.contains(&"set-cookie"));
        assert_eq!(
            VOLATILE_HEADERS, sorted,
            "keep VOLATILE_HEADERS sorted so the docs and the guide stay diffable"
        );
    }
}
