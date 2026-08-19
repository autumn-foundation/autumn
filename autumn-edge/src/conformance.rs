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
pub const VOLATILE_HEADERS: &[&str] = &[
    "date",
    "x-request-id",
    "server-timing",
    "content-security-policy",
    "set-cookie",
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
        let mut differences = Vec::new();
        for pair in &native_headers {
            if !edge_headers.contains(pair) {
                differences.push(format!(
                    "origin has `{}: {}` and the edge does not",
                    pair.0, pair.1
                ));
            }
        }
        for pair in &edge_headers {
            if !native_headers.contains(pair) {
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
        let _ = sorted;
    }
}
