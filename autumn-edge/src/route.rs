//! The edge route table: what an `#[edge]` handler compiles down to.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::wire::WireError;

/// A platform seam an edge route needs the host to mediate.
///
/// Capabilities are declared by the author (`#[edge(needs(kv))]`), travel with
/// the route in [`EdgeRoute::needs`], and are checked against the host's
/// `provided_capabilities` *before* dispatch. A route whose needs the host
/// cannot satisfy falls through to the origin rather than serving a degraded
/// answer — the edge lane never guesses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum EdgeCapability {
    /// A non-authoritative key/value read replica; see [`crate::kv::EdgeKv`].
    Kv,
}

impl EdgeCapability {
    /// The wire spelling of this capability.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Kv => "kv",
        }
    }
}

impl fmt::Display for EdgeCapability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for EdgeCapability {
    type Err = WireError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw {
            "kv" => Ok(Self::Kv),
            other => Err(WireError::UnknownCapability(other.to_owned())),
        }
    }
}

/// The edge router's state type.
///
/// Deliberately a unit: an edge handler that could reach into application
/// state would not be portable, and the seams the edge *does* offer
/// ([`EdgeCache`](crate::extract::EdgeCache)) arrive through request
/// extensions so they work for any state type — at the origin as well as at
/// the edge.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EdgeState;

/// One route in the edge lane.
///
/// Values of this type are produced by the `__autumn_edge_route_*` companion
/// functions the `#[edge]` macro emits, and collected by `edge_routes![]`.
/// Hand-construction is supported but unusual.
pub struct EdgeRoute {
    /// Always [`http::Method::GET`] in wire version 1. `HEAD` is served by the
    /// same handler through axum's `MethodRouter`.
    pub method: http::Method,
    /// The axum path pattern, identical to the origin route's.
    pub path: &'static str,
    /// The handler, already adapted by [`edge_get`](crate::handler::edge_get).
    pub handler: axum::routing::MethodRouter<EdgeState>,
    /// The handler function's name, for diagnostics and build output.
    pub name: &'static str,
    /// Platform seams this route needs; checked pre-dispatch.
    pub needs: &'static [EdgeCapability],
}

impl fmt::Debug for EdgeRoute {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `MethodRouter` carries boxed services with no useful debug output;
        // omitting it keeps this readable in a test failure message.
        f.debug_struct("EdgeRoute")
            .field("method", &self.method)
            .field("path", &self.path)
            .field("name", &self.name)
            .field("needs", &self.needs)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_round_trips_through_its_wire_spelling() {
        assert_eq!(EdgeCapability::Kv.as_str(), "kv");
        assert_eq!(EdgeCapability::Kv.to_string(), "kv");
        assert_eq!(
            "kv".parse::<EdgeCapability>().expect("parses"),
            EdgeCapability::Kv
        );
        assert!(matches!(
            "db".parse::<EdgeCapability>(),
            Err(WireError::UnknownCapability(_))
        ));
    }

    #[test]
    fn capability_serializes_as_a_bare_string() {
        assert_eq!(
            serde_json::to_string(&vec![EdgeCapability::Kv]).expect("serializes"),
            r#"["kv"]"#
        );
        assert_eq!(
            serde_json::from_str::<Vec<EdgeCapability>>(r#"["kv"]"#).expect("parses"),
            vec![EdgeCapability::Kv]
        );
    }

    #[test]
    fn route_debug_omits_the_handler_and_names_the_route() {
        let route = EdgeRoute {
            method: http::Method::GET,
            path: "/greet/{name}",
            handler: crate::handler::edge_get(|| async { "hi" }),
            name: "greet",
            needs: &[EdgeCapability::Kv],
        };

        let rendered = format!("{route:?}");
        assert!(rendered.contains("/greet/{name}"), "{rendered}");
        assert!(rendered.contains("greet"), "{rendered}");
        assert!(rendered.contains("Kv"), "{rendered}");
        assert!(!rendered.contains("MethodRouter"), "{rendered}");
    }
}
